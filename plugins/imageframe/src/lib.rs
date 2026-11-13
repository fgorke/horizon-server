//! Horizon Server plugin (`docs/plugins.md`): images from the web on maps. An operator's
//! `/imageframe create <name> <columns> <rows> <url>` fetches a PNG, JPEG, WebP or GIF and fits
//! it onto that many maps (capability `maps`); `place` hangs them in item frames on the wall the
//! operator looks at, `get` hands them out. A GIF keeps playing on its maps, and `pause` stops
//! it. The maps are the world's, so a picture stays after a restart; an animation's frames are
//! fetched again when the server starts.
use horizon_mod_sdk::{
    command,
    messages::Messages,
    server::{Argument, Player, Registration, ServerMod, maps, players, world},
    storage,
};
use std::collections::{BTreeMap, VecDeque};

const OPERATOR: u8 = 2;
/// The storage key of the images, one a line.
const KEY: &str = "images";
const MAX_NAME: usize = 32;
const MAX_IMAGES: usize = 256;
/// How far `place` looks for a wall, in blocks.
const REACH: f64 = 32.0;
/// The eyes' height above the feet.
const EYES: f64 = 1.62;
/// Requests `maps.fetch` takes at once.
const REQUESTS: usize = 4;
/// Bytes of animation frames the plugin keeps across all images.
const ANIMATION_BUDGET: usize = 32 << 20;
/// A map's picture, in bytes.
const PICTURE: usize = 128 * 128;
/// Pictures painted in a tick: `maps.paint`'s limit per call.
const PAINTS_PER_TICK: usize = 256;
const MS_PER_TICK: u32 = 50;

const MESSAGES: &[(&str, &str)] = &[
    ("not_operator", "You need to be an operator for that."),
    (
        "usage",
        "/imageframe create <name> <columns> <rows> <url> | get <name> | place <name> | list | delete <name> | rename <name> <new name> | pause <name> | refresh <name>",
    ),
    ("bad_name", "A name is 1 to 32 letters, digits, - and _."),
    ("taken", "There is an image called {name} already."),
    ("unknown", "There is no image called {name}."),
    (
        "too_many",
        "There are 256 images already: delete one first.",
    ),
    ("fetching", "Fetching {url} for {name}..."),
    ("failed", "Image {name} failed: {error}"),
    (
        "made",
        "Image {name} is ready on {count} maps. /imageframe place {name} hangs it on the wall you look at, /imageframe get {name} gives you the maps.",
    ),
    ("animated", "Image {name} plays {frames} frames."),
    (
        "too_long",
        "Image {name} is too long to play: it shows its first frame.",
    ),
    ("refreshed", "Image {name} fetched again."),
    ("given", "Gave you the {count} maps of {name}."),
    ("placed", "Hung {name} on the wall."),
    ("no_wall", "Look at a wall within 32 blocks."),
    (
        "no_room",
        "No room for {name} there: {x} {y} {z} needs a wall behind and air in front.",
    ),
    ("none", "No images yet."),
    ("list", "{name}: {columns} x {rows} maps{state}"),
    (
        "deleted",
        "Deleted {name}; its maps keep their last picture.",
    ),
    ("renamed", "Renamed {name} to {new}."),
    ("paused", "Paused {name}."),
    ("playing", "Playing {name}."),
    ("still", "{name} is no animation."),
];

/// An image's maps and where it came from.
#[derive(Clone, Debug, PartialEq)]
struct Image {
    name: String,
    url: String,
    columns: u32,
    rows: u32,
    /// Row by row from the top left.
    maps: Vec<i32>,
    animated: bool,
    paused: bool,
}

impl Image {
    /// One storage line: tab-separated, the address last.
    fn line(&self) -> String {
        let maps: Vec<String> = self.maps.iter().map(i32::to_string).collect();
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.name,
            self.columns,
            self.rows,
            u8::from(self.animated),
            u8::from(self.paused),
            maps.join(","),
            self.url
        )
    }

    fn parse(line: &str) -> Option<Image> {
        let mut parts = line.splitn(7, '\t');
        let name = parts.next()?.to_string();
        let columns = parts.next()?.parse().ok()?;
        let rows = parts.next()?.parse().ok()?;
        let animated = parts.next()? == "1";
        let paused = parts.next()? == "1";
        let maps: Vec<i32> = parts
            .next()?
            .split(',')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        let url = parts.next()?.to_string();
        (name_ok(&name) && maps.len() == (columns * rows) as usize).then_some(Image {
            name,
            url,
            columns,
            rows,
            maps,
            animated,
            paused,
        })
    }
}

fn name_ok(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A fetch to make or being made.
#[derive(Debug)]
struct Job {
    /// The player to tell, by id; empty for the fetches at start.
    player: String,
    name: String,
    url: String,
    columns: u32,
    rows: u32,
    /// The new image's dimension; none to fetch an image's frames again.
    create: Option<String>,
    request: Option<u32>,
}

/// An animation playing on an image's maps.
struct Animation {
    frames: Vec<maps::Frame>,
    at: usize,
    /// The tick its next frame shows.
    due: u64,
}

impl Animation {
    fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.pictures.len() * PICTURE).sum()
    }
}

struct ImageFrame {
    images: Vec<Image>,
    animations: BTreeMap<String, Animation>,
    jobs: VecDeque<Job>,
    messages: Messages,
    tick: u64,
}

/// A wall's block and the way its face looks, a unit step along x or z.
type Wall = ([i32; 3], [i32; 3]);

/// Air and what an item frame may hang in.
fn is_air(state: &str) -> bool {
    let name = state.split('[').next().unwrap_or_default();
    matches!(
        name,
        "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
    )
}

/// What a look passes through: air, water and light blocks.
fn see_through(state: &str) -> bool {
    let name = state.split('[').next().unwrap_or_default();
    is_air(state) || matches!(name, "minecraft:water" | "minecraft:light")
}

/// The first block a look from `eye` along `yaw` and `pitch` (degrees, as F3 shows them) meets
/// within [`REACH`], and the face it meets: blocks through `block` (none for one not loaded).
fn look_at(
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    block: impl Fn([i32; 3]) -> Option<String>,
) -> Option<Wall> {
    let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    let dir = [
        -yaw.sin() * pitch.cos(),
        -pitch.sin(),
        yaw.cos() * pitch.cos(),
    ];
    let mut at = eye.map(|v| v.floor() as i32);
    let step = dir.map(|d| if d > 0.0 { 1 } else { -1 });
    let next = |i: usize| {
        if dir[i] == 0.0 {
            return f64::INFINITY;
        }
        let edge = f64::from(at[i]) + if dir[i] > 0.0 { 1.0 } else { 0.0 };
        (edge - eye[i]) / dir[i]
    };
    let mut t_max = [next(0), next(1), next(2)];
    let t_delta = dir.map(|d| {
        if d == 0.0 {
            f64::INFINITY
        } else {
            1.0 / d.abs()
        }
    });
    loop {
        let axis = (0..3).min_by(|&a, &b| t_max[a].total_cmp(&t_max[b]))?;
        if t_max[axis] > REACH {
            return None;
        }
        at[axis] += step[axis];
        t_max[axis] += t_delta[axis];
        if !see_through(&block(at)?) {
            let mut face = [0; 3];
            face[axis] = -step[axis];
            return Some((at, face));
        }
    }
}

/// Where the frames of a `columns` x `rows` image go on `wall` (its top left block): each
/// frame's block and the map's index, row by row; to the viewer's right and down.
fn layout(wall: Wall, columns: u32, rows: u32) -> Vec<([i32; 3], [i32; 3])> {
    let (top_left, face) = wall;
    let right = [face[2], 0, -face[0]];
    let mut spots = Vec::new();
    for row in 0..rows as i32 {
        for col in 0..columns as i32 {
            let wall = [
                top_left[0] + right[0] * col,
                top_left[1] - row,
                top_left[2] + right[2] * col,
            ];
            spots.push((wall, [wall[0] + face[0], wall[1], wall[2] + face[2]]));
        }
    }
    spots
}

/// The `Facing` of an item frame on a face: north 2, south 3, west 4, east 5.
fn facing(face: [i32; 3]) -> u8 {
    match face {
        [0, 0, -1] => 2,
        [0, 0, 1] => 3,
        [-1, 0, 0] => 4,
        _ => 5,
    }
}

fn summon(dimension: &str, at: [i32; 3], face: [i32; 3], map: i32) -> String {
    format!(
        "execute in {dimension} run summon minecraft:item_frame {} {} {} {{Facing:{}b,Invisible:1b,Item:{{id:\"minecraft:filled_map\",count:1,components:{{\"minecraft:map_id\":{map}}}}}}}",
        f64::from(at[0]) + 0.5,
        f64::from(at[1]) + 0.5,
        f64::from(at[2]) + 0.5,
        facing(face)
    )
}

impl ImageFrame {
    fn save(&self) {
        let lines: Vec<String> = self.images.iter().map(Image::line).collect();
        if let Err(e) = storage::set(KEY, lines.join("\n").as_bytes()) {
            horizon_mod_sdk::warn(&format!("Couldn't save the images: {e}"));
        }
    }

    fn find(&self, name: &str) -> Result<usize, String> {
        self.images
            .iter()
            .position(|i| i.name == name)
            .ok_or_else(|| self.messages.get("unknown", &[("name", &name)]))
    }

    fn tell(&self, player: &str, key: &str, values: &[(&str, &dyn std::fmt::Display)]) {
        if !player.is_empty() {
            players::tell(player, &self.messages.get(key, values));
        }
    }

    fn create(&mut self, player: &Player, words: &[&str]) -> Result<String, String> {
        let [name, columns, rows, url] = words else {
            return Err(self.messages.get("usage", &[]));
        };
        if !name_ok(name) {
            return Err(self.messages.get("bad_name", &[]));
        }
        let taken = self.images.iter().any(|i| i.name == *name)
            || self.jobs.iter().any(|j| j.name == *name);
        if taken {
            return Err(self.messages.get("taken", &[("name", name)]));
        }
        if self.images.len() >= MAX_IMAGES {
            return Err(self.messages.get("too_many", &[]));
        }
        let (Ok(columns), Ok(rows)) = (columns.parse::<u32>(), rows.parse::<u32>()) else {
            return Err(self.messages.get("usage", &[]));
        };
        self.jobs.push_back(Job {
            player: player.id.clone(),
            name: (*name).to_string(),
            url: (*url).to_string(),
            columns,
            rows,
            create: Some(player.dimension.clone()),
            request: None,
        });
        self.start_jobs()?;
        Ok(self
            .messages
            .get("fetching", &[("url", url), ("name", name)]))
    }

    /// Starts the waiting fetches there's room for. An error is the newest job's, which goes.
    fn start_jobs(&mut self) -> Result<(), String> {
        let running = self.jobs.iter().filter(|j| j.request.is_some()).count();
        let mut room = REQUESTS.saturating_sub(running);
        let mut failed = None;
        for job in self.jobs.iter_mut().filter(|j| j.request.is_none()) {
            if room == 0 {
                break;
            }
            match maps::fetch(&job.url, job.columns, job.rows) {
                Ok(request) => {
                    job.request = Some(request);
                    room -= 1;
                }
                Err(e) => {
                    failed = Some((job.name.clone(), e));
                    break;
                }
            }
        }
        match failed {
            Some((name, e)) => {
                self.jobs.retain(|j| j.name != name || j.request.is_some());
                Err(self
                    .messages
                    .get("failed", &[("name", &name), ("error", &e)]))
            }
            None => Ok(()),
        }
    }

    /// The fetches that are done.
    fn collect(&mut self) {
        let mut done = Vec::new();
        for (i, job) in self.jobs.iter().enumerate() {
            if let Some(outcome) = job.request.and_then(maps::fetched) {
                done.push((i, outcome));
            }
        }
        for (i, outcome) in done.into_iter().rev() {
            let Some(job) = self.jobs.remove(i) else {
                continue;
            };
            match outcome {
                Ok(frames) => self.finish(&job, frames),
                Err(e) => self.tell(&job.player, "failed", &[("name", &job.name), ("error", &e)]),
            }
        }
        if let Err(e) = self.start_jobs() {
            horizon_mod_sdk::warn(&e);
        }
    }

    fn finish(&mut self, job: &Job, frames: Vec<maps::Frame>) {
        let Some(first) = frames.first() else {
            return;
        };
        let name = job.name.as_str();
        let index = match &job.create {
            Some(dimension) => {
                let made: Result<Vec<i32>, String> = first
                    .pictures
                    .iter()
                    .map(|p| maps::create(dimension, p))
                    .collect();
                let maps = match made {
                    Ok(maps) => maps,
                    Err(e) => {
                        self.tell(&job.player, "failed", &[("name", &name), ("error", &e)]);
                        return;
                    }
                };
                self.images.push(Image {
                    name: name.to_string(),
                    url: job.url.clone(),
                    columns: job.columns,
                    rows: job.rows,
                    maps,
                    animated: false,
                    paused: false,
                });
                let count = job.columns * job.rows;
                self.tell(&job.player, "made", &[("name", &name), ("count", &count)]);
                self.images.len() - 1
            }
            None => {
                let Ok(index) = self.find(name) else {
                    return;
                };
                let image = &self.images[index];
                if first.pictures.len() != image.maps.len() {
                    return;
                }
                for (id, picture) in image.maps.iter().zip(&first.pictures) {
                    if let Err(e) = maps::paint(*id, picture) {
                        horizon_mod_sdk::warn(&format!("Image {name}: {e}"));
                    }
                }
                self.tell(&job.player, "refreshed", &[("name", &name)]);
                index
            }
        };
        self.animations.remove(name);
        let animation = Animation {
            due: self.tick + u64::from(first.delay_ms / MS_PER_TICK).max(1),
            frames,
            at: 0,
        };
        let animated = animation.frames.len() > 1;
        let used: usize = self.animations.values().map(Animation::bytes).sum();
        if animated && used + animation.bytes() > ANIMATION_BUDGET {
            self.tell(&job.player, "too_long", &[("name", &name)]);
        } else if animated {
            let frames = animation.frames.len();
            self.tell(
                &job.player,
                "animated",
                &[("name", &name), ("frames", &frames)],
            );
            self.animations.insert(name.to_string(), animation);
        }
        self.images[index].animated = animated;
        self.save();
    }

    /// Shows the frames that are due, up to [`PAINTS_PER_TICK`] pictures.
    fn play(&mut self) {
        let mut painted = 0;
        for image in self.images.iter().filter(|i| !i.paused) {
            let Some(animation) = self.animations.get_mut(&image.name) else {
                continue;
            };
            if animation.due > self.tick || painted + image.maps.len() > PAINTS_PER_TICK {
                continue;
            }
            animation.at = (animation.at + 1) % animation.frames.len();
            let frame = &animation.frames[animation.at];
            for (id, picture) in image.maps.iter().zip(&frame.pictures) {
                if maps::paint(*id, picture).is_ok() {
                    painted += 1;
                }
            }
            animation.due = self.tick + u64::from(frame.delay_ms / MS_PER_TICK).max(1);
        }
    }

    fn give(&self, player: &Player, name: &str) -> Result<String, String> {
        let image = &self.images[self.find(name)?];
        let lines: Vec<String> = image
            .maps
            .iter()
            .map(|id| {
                format!(
                    "give {} minecraft:filled_map[minecraft:map_id={id}]",
                    player.name
                )
            })
            .collect();
        command::run_quiet(&lines);
        let count = image.maps.len();
        Ok(self
            .messages
            .get("given", &[("count", &count), ("name", &name)]))
    }

    fn place(&self, player: &Player, name: &str) -> Result<String, String> {
        let image = &self.images[self.find(name)?];
        let (yaw, pitch) = players::rotation(&player.id).unwrap_or((0.0, 0.0));
        let dimension = player.dimension.as_str();
        let block = |at: [i32; 3]| world::get_block(dimension, at[0], at[1], at[2]);
        let eye = [player.x, player.y + EYES, player.z];
        let wall = look_at(eye, yaw, pitch, block)
            .filter(|(_, face)| face[1] == 0)
            .ok_or_else(|| self.messages.get("no_wall", &[]))?;
        let spots = layout(wall, image.columns, image.rows);
        for (behind, at) in &spots {
            let solid = block(*behind).is_some_and(|s| !see_through(&s));
            if !solid || !block(*at).is_some_and(|s| is_air(&s)) {
                let values: [(&str, &dyn std::fmt::Display); 4] =
                    [("name", &name), ("x", &at[0]), ("y", &at[1]), ("z", &at[2])];
                return Err(self.messages.get("no_room", &values));
            }
        }
        let lines: Vec<String> = spots
            .iter()
            .zip(&image.maps)
            .map(|((_, at), id)| summon(dimension, *at, wall.1, *id))
            .collect();
        command::run_quiet(&lines);
        Ok(self.messages.get("placed", &[("name", &name)]))
    }

    fn list(&self) -> String {
        if self.images.is_empty() {
            return self.messages.get("none", &[]);
        }
        let lines: Vec<String> = self
            .images
            .iter()
            .map(|i| {
                let state = match (i.animated, i.paused) {
                    (true, true) => ", paused",
                    (true, false) => ", playing",
                    _ => "",
                };
                let values: [(&str, &dyn std::fmt::Display); 4] = [
                    ("name", &i.name),
                    ("columns", &i.columns),
                    ("rows", &i.rows),
                    ("state", &state),
                ];
                self.messages.get("list", &values)
            })
            .collect();
        lines.join("\n")
    }

    fn delete(&mut self, name: &str) -> Result<String, String> {
        let index = self.find(name)?;
        self.images.remove(index);
        self.animations.remove(name);
        self.save();
        Ok(self.messages.get("deleted", &[("name", &name)]))
    }

    fn rename(&mut self, name: &str, new: &str) -> Result<String, String> {
        let index = self.find(name)?;
        if !name_ok(new) {
            return Err(self.messages.get("bad_name", &[]));
        }
        if self.images.iter().any(|i| i.name == new) {
            return Err(self.messages.get("taken", &[("name", &new)]));
        }
        self.images[index].name = new.to_string();
        if let Some(animation) = self.animations.remove(name) {
            self.animations.insert(new.to_string(), animation);
        }
        self.save();
        Ok(self
            .messages
            .get("renamed", &[("name", &name), ("new", &new)]))
    }

    fn pause(&mut self, name: &str) -> Result<String, String> {
        let index = self.find(name)?;
        let image = &mut self.images[index];
        if !image.animated {
            return Err(self.messages.get("still", &[("name", &name)]));
        }
        image.paused = !image.paused;
        let key = if image.paused { "paused" } else { "playing" };
        self.save();
        Ok(self.messages.get(key, &[("name", &name)]))
    }

    fn refresh(&mut self, player: &Player, name: &str) -> Result<String, String> {
        let image = &self.images[self.find(name)?];
        let url = image.url.clone();
        self.jobs.push_back(Job {
            player: player.id.clone(),
            name: name.to_string(),
            url: url.clone(),
            columns: image.columns,
            rows: image.rows,
            create: None,
            request: None,
        });
        self.start_jobs()?;
        Ok(self
            .messages
            .get("fetching", &[("url", &url), ("name", &name)]))
    }
}

impl ImageFrame {
    /// What the chat box offers: the actions, and the images' names.
    fn suggest(&self) {
        let actions = [
            "create", "get", "place", "list", "delete", "rename", "pause", "refresh",
        ]
        .map(str::to_owned);
        players::set_suggestions("imageframe", "action", &actions);
        let names: Vec<String> = self.images.iter().map(|i| i.name.clone()).collect();
        players::set_suggestions("imageframe", "name", &names);
    }
}

impl ServerMod for ImageFrame {
    fn init() -> Result<(Self, Registration), String> {
        let images: Vec<Image> = storage::get(KEY)
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .lines()
                    .filter_map(Image::parse)
                    .collect()
            })
            .unwrap_or_default();
        // An animation's frames live only in memory: fetch them again.
        let jobs = images
            .iter()
            .filter(|i| i.animated)
            .map(|i| Job {
                player: String::new(),
                name: i.name.clone(),
                url: i.url.clone(),
                columns: i.columns,
                rows: i.rows,
                create: None,
                request: None,
            })
            .collect();
        Ok((
            ImageFrame {
                images,
                animations: BTreeMap::new(),
                jobs,
                messages: Messages::load(MESSAGES),
                tick: 0,
            },
            Registration::new()
                .command_with(
                    "imageframe",
                    [
                        Argument::word("action").optional(),
                        Argument::word("name").optional(),
                        Argument::text("rest").optional(),
                    ],
                )
                .on_tick(),
        ))
    }

    fn command(
        &mut self,
        player: &Player,
        _name: &str,
        args: &horizon_mod_sdk::server::Args,
    ) -> Result<Option<String>, String> {
        if players::permission(&player.id).unwrap_or(0) < OPERATOR {
            return Err(self.messages.get("not_operator", &[]));
        }
        let words: Vec<&str> = args.text().split_whitespace().collect();
        let reply = match words.as_slice() {
            ["create", rest @ ..] => self.create(player, rest),
            ["get", name] => self.give(player, name),
            ["place", name] => self.place(player, name),
            ["list"] => Ok(self.list()),
            ["delete", name] => self.delete(name),
            ["rename", name, new] => self.rename(name, new),
            ["pause", name] => self.pause(name),
            ["refresh", name] => self.refresh(player, name),
            _ => Err(self.messages.get("usage", &[])),
        };
        reply.map(Some)
    }

    fn tick(&mut self, tick: u64) {
        self.tick = tick;
        if tick.is_multiple_of(20) {
            self.suggest();
        }
        if !self.jobs.is_empty() {
            self.collect();
        }
        self.play();
    }
}

horizon_mod_sdk::export_server_mod!(ImageFrame);

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Image {
        Image {
            name: "logo".into(),
            url: "https://example.com/a b.gif".into(),
            columns: 2,
            rows: 1,
            maps: vec![7, 8],
            animated: true,
            paused: false,
        }
    }

    #[test]
    fn an_image_survives_its_storage_line() {
        let i = image();
        assert_eq!(Image::parse(&i.line()), Some(i));
        assert_eq!(Image::parse("bad"), None);
        assert_eq!(
            Image::parse("x\t2\t2\t0\t0\t1,2\thttps://a"),
            None,
            "4 maps wanted"
        );
    }

    #[test]
    fn names_are_plain_words() {
        assert!(name_ok("Logo_2-b"));
        assert!(!name_ok(""));
        assert!(!name_ok("a/b"));
        assert!(!name_ok(&"x".repeat(33)));
    }

    /// A stone wall at z = 5, air elsewhere.
    fn wall(at: [i32; 3]) -> Option<String> {
        Some(
            if at[2] == 5 {
                "minecraft:stone"
            } else {
                "minecraft:air"
            }
            .into(),
        )
    }

    #[test]
    fn a_look_south_meets_the_walls_north_face() {
        // Yaw 0 looks south (+z).
        let hit = look_at([0.5, 65.62, 0.5], 0.0, 0.0, wall);
        assert_eq!(hit, Some(([0, 65, 5], [0, 0, -1])));
        // Straight up meets nothing within reach.
        assert_eq!(look_at([0.5, 65.62, 0.5], 0.0, -90.0, wall), None);
        // Nothing loaded: nothing found.
        assert_eq!(look_at([0.5, 65.62, 0.5], 0.0, 0.0, |_| None), None);
    }

    #[test]
    fn frames_go_right_and_down_from_the_top_left() {
        // Facing north, the viewer looks south: their right is west (-x).
        let spots = layout(([0, 65, 5], [0, 0, -1]), 2, 2);
        assert_eq!(
            spots,
            vec![
                ([0, 65, 5], [0, 65, 4]),
                ([-1, 65, 5], [-1, 65, 4]),
                ([0, 64, 5], [0, 64, 4]),
                ([-1, 64, 5], [-1, 64, 4]),
            ]
        );
        // On an east face the viewer looks west: their right is north (-z).
        let spots = layout(([3, 70, 0], [1, 0, 0]), 2, 1);
        assert_eq!(spots[1].1, [4, 70, -1]);
    }

    #[test]
    fn a_frame_faces_out_of_its_wall() {
        assert_eq!(facing([0, 0, -1]), 2);
        assert_eq!(facing([0, 0, 1]), 3);
        assert_eq!(facing([-1, 0, 0]), 4);
        assert_eq!(facing([1, 0, 0]), 5);
        assert_eq!(
            summon("minecraft:overworld", [1, 2, -3], [0, 0, 1], 9),
            "execute in minecraft:overworld run summon minecraft:item_frame 1.5 2.5 -2.5 {Facing:3b,Invisible:1b,Item:{id:\"minecraft:filled_map\",count:1,components:{\"minecraft:map_id\":9}}}"
        );
    }
}
