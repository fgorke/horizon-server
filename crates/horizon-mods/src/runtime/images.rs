//! Capability `maps` (server plugins): pictures on maps. An image fetched from the web (PNG,
//! JPEG, WebP or GIF, an animation frame by frame) is fitted onto a wall of maps without
//! stretching and dithered to the map colours on a thread of its own; the plugin asks for the
//! outcome between ticks (`maps.fetched`). New maps and new pictures reach the game as
//! [`Output::NewMap`] and [`Output::PaintMap`], made after the call returns.
use super::{Output, State};
use std::{
    collections::HashMap,
    io::{Cursor, Read},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicI32, Ordering},
    },
    time::Duration,
};

/// A map's side, in pixels, and its picture's size.
pub const MAP_SIDE: usize = 128;
pub const PICTURE: usize = MAP_SIDE * MAP_SIDE;
/// The most maps a picture spans across or down, and in all.
const MAX_SPAN: u32 = 16;
const MAX_MAPS: u32 = 64;
/// Requests a plugin may have running (or done and not yet asked for).
const MAX_REQUESTS: usize = 4;
/// `maps.create` and `maps.paint` per call.
const CREATES_PER_CALL: usize = 64;
const PAINTS_PER_CALL: usize = 256;
/// A fetched file's size, how long the fetch may take, and redirects followed.
const MAX_BYTES: u64 = 16 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const REDIRECTS: u32 = 5;
const MAX_URL: usize = 2048;
/// An image's side, and the pixels decoded across an animation's frames.
const MAX_SIDE: u32 = 8192;
const MAX_DECODED: u64 = 1 << 30;
/// Bytes of pictures one outcome holds (frames x maps x [`PICTURE`]): well within a plugin's
/// memory. A longer animation keeps every second frame (then every fourth...), each shown
/// as long as those it stands for.
const MAX_RESULT: usize = 8 << 20;
/// The shortest frame: a tick. A GIF frame of 10 ms or less shows for 100 ms, as browsers do.
pub const MIN_DELAY_MS: u32 = 50;
const GIF_DEFAULT_DELAY_MS: u32 = 100;
/// Frames read before the rest are dropped.
const MAX_FRAMES: usize = 2048;

/// `MapColor`'s colours by id, as `0xRRGGBB` (id 0 is transparent): the same table as
/// `horizon_items::blocks::MAP_COLORS`, which the game's tests hold this to.
pub const MAP_COLORS: [u32; 62] = [
    0, 8368696, 16247203, 13092807, 16711680, 10526975, 10987431, 31744, 16777215, 10791096,
    9923917, 7368816, 4210943, 9402184, 16776437, 14188339, 11685080, 6724056, 15066419, 8375321,
    15892389, 5000268, 10066329, 5013401, 8339378, 3361970, 6704179, 6717235, 10040115, 1644825,
    16445005, 6085589, 4882687, 55610, 8476209, 7340544, 13742497, 10441252, 9787244, 7367818,
    12223780, 6780213, 10505550, 3746083, 8874850, 5725276, 8014168, 4996700, 4993571, 5001770,
    9321518, 2430480, 12398641, 9715553, 6035741, 1474182, 3837580, 5647422, 1356933, 6579300,
    14200723, 8365974,
];
/// `MapColor.Brightness`'s modifiers, by shade.
const SHADES: [u32; 4] = [180, 220, 255, 135];

/// One frame of a fetched image: a picture per map, row by row, and how long it shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub delay_ms: u32,
    pub pictures: Vec<Vec<u8>>,
}

type Outcome = Result<Vec<Frame>, String>;
type Slot = Arc<Mutex<Option<Outcome>>>;

/// One plugin's requests, and the world's next map id (shared by every plugin, set by the
/// game before its calls).
#[derive(Default)]
pub(super) struct Fetches {
    running: HashMap<u32, Slot>,
    next: u32,
    map_ids: Arc<AtomicI32>,
    /// This call's creates and paints.
    creates: usize,
    paints: usize,
}

impl Fetches {
    pub(super) fn new(map_ids: Arc<AtomicI32>) -> Self {
        Fetches {
            map_ids,
            ..Fetches::default()
        }
    }
    pub(super) fn new_call(&mut self) {
        self.creates = 0;
        self.paints = 0;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `namespace:path`, as a dimension's id.
fn dimension_ok(dimension: &str) -> bool {
    let plain = |t: &str, extra: &[u8]| {
        !t.is_empty()
            && t.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b"_.-".contains(&b)
                    || extra.contains(&b)
            })
    };
    dimension.len() <= 256
        && dimension
            .split_once(':')
            .is_some_and(|(ns, path)| plain(ns, b"") && plain(path, b"/"))
}

/// A picture is [`PICTURE`] packed map colours, each a known id times 4 plus a shade.
fn picture_ok(picture: &[u8]) -> Result<(), String> {
    if picture.len() != PICTURE {
        return Err(format!(
            "a picture is {PICTURE} map colours, not {}",
            picture.len()
        ));
    }
    match picture
        .iter()
        .find(|&&c| usize::from(c >> 2) >= MAP_COLORS.len())
    {
        Some(c) => Err(format!("{c} is no map colour")),
        None => Ok(()),
    }
}

impl super::plugin_io::maps::Host for State {
    fn fetch(&mut self, url: String, columns: u32, rows: u32) -> Result<u32, String> {
        check_span(columns, rows)?;
        check_url(&url)?;
        let fetches = &mut self.io.images;
        if fetches.running.len() >= MAX_REQUESTS {
            return Err(format!("{MAX_REQUESTS} images are being fetched already"));
        }
        let slot: Slot = Arc::new(Mutex::new(None));
        if self.io.simulated {
            *lock(&slot) = Some(Err("images are not fetched in tests".into()));
        } else {
            let out = Arc::clone(&slot);
            let id = self.id.clone();
            std::thread::Builder::new()
                .name(format!("plugin {id} image"))
                .spawn(move || {
                    let outcome = fetch(&url).and_then(|bytes| render(&bytes, columns, rows));
                    if let Err(e) = &outcome {
                        log::info!("plugin {id}: image {url}: {e}");
                    }
                    *lock(&out) = Some(outcome);
                })
                .map_err(|e| e.to_string())?;
        }
        let fetches = &mut self.io.images;
        fetches.next = fetches.next.wrapping_add(1);
        fetches.running.insert(fetches.next, slot);
        Ok(fetches.next)
    }

    fn fetched(
        &mut self,
        request: u32,
    ) -> Option<Result<Vec<super::plugin_io::maps::Frame>, String>> {
        let running = &mut self.io.images.running;
        let outcome = lock(running.get(&request)?).take()?;
        running.remove(&request);
        Some(outcome.map(|frames| {
            frames
                .into_iter()
                .map(|f| super::plugin_io::maps::Frame {
                    delay_ms: f.delay_ms,
                    pictures: f.pictures,
                })
                .collect()
        }))
    }

    fn create(&mut self, dimension: String, picture: Vec<u8>) -> Result<i32, String> {
        if !dimension_ok(&dimension) {
            return Err(format!("{dimension} names no dimension"));
        }
        picture_ok(&picture)?;
        let fetches = &mut self.io.images;
        if fetches.creates >= CREATES_PER_CALL {
            return Err(format!("at most {CREATES_PER_CALL} maps a call"));
        }
        fetches.creates += 1;
        let id = fetches.map_ids.fetch_add(1, Ordering::Relaxed);
        self.out.push(Output::NewMap {
            id,
            dimension,
            colors: picture,
        });
        Ok(id)
    }

    fn paint(&mut self, id: i32, picture: Vec<u8>) -> Result<(), String> {
        picture_ok(&picture)?;
        let fetches = &mut self.io.images;
        if fetches.paints >= PAINTS_PER_CALL {
            return Err(format!("at most {PAINTS_PER_CALL} pictures a call"));
        }
        fetches.paints += 1;
        self.out.push(Output::PaintMap {
            id,
            colors: picture,
        });
        Ok(())
    }
}

fn check_span(columns: u32, rows: u32) -> Result<(), String> {
    let ok = (1..=MAX_SPAN).contains(&columns)
        && (1..=MAX_SPAN).contains(&rows)
        && columns * rows <= MAX_MAPS;
    ok.then_some(()).ok_or_else(|| {
        format!("{columns} x {rows} maps: 1 to {MAX_SPAN} across and down, {MAX_MAPS} in all")
    })
}

fn check_url(url: &str) -> Result<(), String> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if url.len() > MAX_URL || !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("an image's address starts with http:// or https://".into());
    }
    Ok(())
}

/// The bytes at `url`: public addresses only (every redirect resolved again), up to
/// [`MAX_BYTES`] and [`TIMEOUT`].
fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(TIMEOUT)
        .redirects(REDIRECTS)
        .resolver(public_addresses)
        .user_agent("Horizon-Server image fetch")
        .build();
    let response = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("the server answered {code}"),
        ureq::Error::Transport(t) => t.to_string(),
    })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("the image is over {} MB", MAX_BYTES >> 20));
    }
    Ok(bytes)
}

/// A host's addresses, without those of this machine and private networks: a plugin can't
/// reach what only the server can.
fn public_addresses(netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
    let public: Vec<SocketAddr> = netloc
        .to_socket_addrs()?
        .filter(|a| is_public(a.ip()))
        .collect();
    if public.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "images come only from public addresses",
        ));
    }
    Ok(public)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || s[0] == 0x2002
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x0064 && s[1] == 0xff9b)
                || s[..6] == [0; 6])
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 0))
}

/// An image's frames, fitted and dithered as they are decoded.
struct Frames {
    columns: usize,
    rows: usize,
    frames: Vec<Frame>,
    /// Frames read, and how many each kept frame stands for.
    seen: usize,
    stride: usize,
    decoded: u64,
}

impl Frames {
    fn new(columns: u32, rows: u32) -> Self {
        Frames {
            columns: columns as usize,
            rows: rows as usize,
            frames: Vec::new(),
            seen: 0,
            stride: 1,
            decoded: 0,
        }
    }
    /// False once no more frames are wanted.
    fn wants_more(&self) -> bool {
        self.seen < MAX_FRAMES && self.decoded < MAX_DECODED
    }
    /// The next frame, RGBA `width` x `height`, shown `delay_ms`.
    fn push(&mut self, width: u32, height: u32, rgba: &[u8], delay_ms: u32) {
        self.decoded += u64::from(width) * u64::from(height);
        let delay_ms = delay_ms.max(MIN_DELAY_MS);
        let keep = self.seen.is_multiple_of(self.stride);
        self.seen += 1;
        if !keep {
            if let Some(last) = self.frames.last_mut() {
                last.delay_ms = last.delay_ms.saturating_add(delay_ms);
            }
            return;
        }
        let canvas = fit(
            width,
            height,
            rgba,
            self.columns * MAP_SIDE,
            self.rows * MAP_SIDE,
        );
        let colors = dither(&canvas, self.columns * MAP_SIDE);
        self.frames.push(Frame {
            delay_ms,
            pictures: split(&colors, self.columns, self.rows),
        });
        let maps = self.columns * self.rows;
        if self.frames.len() * maps * PICTURE > MAX_RESULT {
            self.halve();
        }
    }
    /// Every second frame, each shown for both.
    fn halve(&mut self) {
        let frames = std::mem::take(&mut self.frames);
        let mut kept: Vec<Frame> = Vec::with_capacity(frames.len() / 2 + 1);
        for (i, f) in frames.into_iter().enumerate() {
            match kept.last_mut() {
                Some(last) if i % 2 == 1 => {
                    last.delay_ms = last.delay_ms.saturating_add(f.delay_ms);
                }
                _ => kept.push(f),
            }
        }
        self.frames = kept;
        self.stride *= 2;
    }
}

/// The image in `bytes` on `columns` x `rows` maps, frame by frame.
pub fn render(bytes: &[u8], columns: u32, rows: u32) -> Outcome {
    check_span(columns, rows)?;
    let mut frames = Frames::new(columns, rows);
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => decode_png(bytes, &mut frames)?,
        [0xFF, 0xD8, 0xFF, ..] => decode_jpeg(bytes, &mut frames)?,
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => {
            decode_webp(bytes, &mut frames)?;
        }
        [b'G', b'I', b'F', b'8', ..] => decode_gif(bytes, &mut frames)?,
        _ => return Err("not a PNG, JPEG, WebP or GIF image".into()),
    }
    if frames.frames.is_empty() {
        return Err("the image has no frames".into());
    }
    if frames.frames.len() == 1 {
        frames.frames[0].delay_ms = 0;
    }
    Ok(frames.frames)
}

fn check_size(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!(
            "a {width} x {height} image: at most {MAX_SIDE} pixels a side"
        ));
    }
    Ok(())
}

fn decode_png(bytes: &[u8], frames: &mut Frames) -> Result<(), String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (width, height) = (reader.info().width, reader.info().height);
    check_size(width, height)?;
    let mut buffer = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("the image is too large")?
    ];
    let frame = reader.next_frame(&mut buffer).map_err(|e| e.to_string())?;
    let data = &buffer[..frame.buffer_size()];
    let rgba: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("an indexed PNG the decoder didn't expand".into()),
    };
    frames.push(width, height, &rgba, 0);
    Ok(())
}

fn decode_jpeg(bytes: &[u8], frames: &mut Frames) -> Result<(), String> {
    let mut decoder = zune_jpeg::JpegDecoder::new(bytes);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("the JPEG states no size")?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    check_size(width, height)?;
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let count = width as usize * height as usize;
    let rgba: Vec<u8> = match pixels.len().checked_div(count).unwrap_or(0) {
        3 => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        1 => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        _ => return Err("the JPEG decoded to an unexpected channel count".into()),
    };
    frames.push(width, height, &rgba, 0);
    Ok(())
}

fn decode_webp(bytes: &[u8], frames: &mut Frames) -> Result<(), String> {
    let mut decoder =
        image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    check_size(width, height)?;
    let alpha = decoder.has_alpha();
    let to_rgba = |buffer: &[u8]| -> Vec<u8> {
        if alpha {
            buffer.to_vec()
        } else {
            buffer
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect()
        }
    };
    let mut buffer = vec![
        0;
        decoder
            .output_buffer_size()
            .ok_or("the image is too large")?
    ];
    if !decoder.is_animated() {
        decoder.read_image(&mut buffer).map_err(|e| e.to_string())?;
        frames.push(width, height, &to_rgba(&buffer), 0);
        return Ok(());
    }
    while frames.wants_more() {
        match decoder.read_frame(&mut buffer) {
            Ok(delay) => frames.push(width, height, &to_rgba(&buffer), delay),
            Err(image_webp::DecodingError::NoMoreFrames) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn decode_gif(bytes: &[u8], frames: &mut Frames) -> Result<(), String> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    if let Some(limit) = std::num::NonZeroU64::new(64 << 20) {
        options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    }
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|e| e.to_string())?;
    let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    check_size(width, height)?;
    let (w, h) = (width as usize, height as usize);
    let mut canvas = vec![0u8; w * h * 4];
    while frames.wants_more() {
        let Some(frame) = decoder.read_next_frame().map_err(|e| e.to_string())? else {
            break;
        };
        let (left, top) = (usize::from(frame.left), usize::from(frame.top));
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        let dispose = frame.dispose;
        let delay = match u32::from(frame.delay) * 10 {
            d if d <= 10 => GIF_DEFAULT_DELAY_MS,
            d => d,
        };
        let previous = (dispose == gif::DisposalMethod::Previous).then(|| canvas.clone());
        for row in 0..fh.min(h.saturating_sub(top)) {
            for col in 0..fw.min(w.saturating_sub(left)) {
                let from = (row * fw + col) * 4;
                let Some(src) = frame.buffer.get(from..from + 4) else {
                    continue;
                };
                if src[3] != 0 {
                    let to = ((top + row) * w + left + col) * 4;
                    canvas[to..to + 4].copy_from_slice(src);
                }
            }
        }
        frames.push(width, height, &canvas, delay);
        match dispose {
            gif::DisposalMethod::Background => {
                for row in top..(top + fh).min(h) {
                    let from = (row * w + left.min(w)) * 4;
                    let to = (row * w + (left + fw).min(w)) * 4;
                    canvas[from..to].fill(0);
                }
            }
            gif::DisposalMethod::Previous => {
                if let Some(p) = previous {
                    canvas = p;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `rgba` (`width` x `height`) as large as fits on a `cw` x `ch` canvas without stretching,
/// centred, the rest transparent. Shrinking averages the pixels each canvas pixel covers;
/// growing repeats them, so pixel art stays sharp.
fn fit(width: u32, height: u32, rgba: &[u8], cw: usize, ch: usize) -> Vec<[u8; 4]> {
    let (w, h) = (width as usize, height as usize);
    let scale = (cw as f64 / w as f64).min(ch as f64 / h as f64);
    let dw = ((w as f64 * scale).round() as usize).clamp(1, cw);
    let dh = ((h as f64 * scale).round() as usize).clamp(1, ch);
    let (ox, oy) = ((cw - dw) / 2, (ch - dh) / 2);
    let mut canvas = vec![[0u8; 4]; cw * ch];
    let span = |t: usize, d: usize, s: usize| {
        let from = t * s / d;
        let to = ((t + 1) * s).div_ceil(d).clamp(from + 1, s);
        from..to
    };
    for ty in 0..dh {
        let ys = span(ty, dh, h);
        for tx in 0..dw {
            let xs = span(tx, dw, w);
            let (mut sum, mut alpha, mut n) = ([0u64; 3], 0u64, 0u64);
            for y in ys.clone() {
                for x in xs.clone() {
                    let p = &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    let a = u64::from(p[3]);
                    for c in 0..3 {
                        sum[c] += u64::from(p[c]) * a;
                    }
                    alpha += a;
                    n += 1;
                }
            }
            let pixel = if alpha == 0 {
                [0; 4]
            } else {
                let c = |i: usize| (sum[i] / alpha) as u8;
                [c(0), c(1), c(2), (alpha / n) as u8]
            };
            canvas[(oy + ty) * cw + ox + tx] = pixel;
        }
    }
    canvas
}

/// A packed map colour's RGB.
fn packed_rgb(packed: u8) -> [i32; 3] {
    let col = MAP_COLORS[usize::from(packed >> 2)];
    let modifier = SHADES[usize::from(packed & 3)];
    let ch = |s: u32| ((col >> s & 255) * modifier / 255) as i32;
    [ch(16), ch(8), ch(0)]
}

/// The packed colours a picture may use: every id but transparent, in every shade.
fn palette() -> impl Iterator<Item = u8> {
    (4u8..(MAP_COLORS.len() as u8) * 4).filter(|p| p >> 2 != 0)
}

/// Weighted RGB distance ("redmean"), closer to how eyes compare colours than plain RGB.
fn distance(a: [i32; 3], b: [i32; 3]) -> i64 {
    let mean = i64::from((a[0] + b[0]) / 2);
    let d = [0, 1, 2].map(|i| i64::from(a[i] - b[i]));
    (((512 + mean) * d[0] * d[0]) >> 8) + 4 * d[1] * d[1] + (((767 - mean) * d[2] * d[2]) >> 8)
}

/// The nearest packed colour for every RGB at 5 bits a channel.
fn nearest_table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let colours: Vec<(u8, [i32; 3])> = palette().map(|p| (p, packed_rgb(p))).collect();
        (0..32 * 32 * 32)
            .map(|i: i32| {
                let at = [i >> 10 & 31, i >> 5 & 31, i & 31].map(|c| c * 255 / 31);
                colours
                    .iter()
                    .min_by_key(|(_, rgb)| distance(at, *rgb))
                    .map_or(0, |(p, _)| *p)
            })
            .collect()
    })
}

fn nearest(rgb: [i32; 3]) -> u8 {
    let c = rgb.map(|v| (v.clamp(0, 255) * 31 + 127) / 255);
    nearest_table()[(c[0] << 10 | c[1] << 5 | c[2]) as usize]
}

/// Map colours for a canvas `width` pixels wide, by Floyd-Steinberg error diffusion; a pixel
/// under half opaque is transparent and passes no error on.
fn dither(canvas: &[[u8; 4]], width: usize) -> Vec<u8> {
    let height = canvas.len() / width;
    let mut out = vec![0u8; canvas.len()];
    let mut here = vec![[0i32; 3]; width + 2];
    let mut below = vec![[0i32; 3]; width + 2];
    for y in 0..height {
        for x in 0..width {
            let p = canvas[y * width + x];
            if p[3] < 128 {
                continue;
            }
            let e = here[x + 1];
            let want = [0, 1, 2].map(|i| i32::from(p[i]) + e[i] / 16);
            let packed = nearest(want);
            out[y * width + x] = packed;
            let got = packed_rgb(packed);
            let err = [0, 1, 2].map(|i| want[i].clamp(0, 255) - got[i]);
            for i in 0..3 {
                here[x + 2][i] += err[i] * 7;
                below[x][i] += err[i] * 3;
                below[x + 1][i] += err[i] * 5;
                below[x + 2][i] += err[i];
            }
        }
        std::mem::swap(&mut here, &mut below);
        below.fill([0; 3]);
    }
    out
}

/// A canvas `columns` x `rows` maps big as one picture per map, row by row.
fn split(colors: &[u8], columns: usize, rows: usize) -> Vec<Vec<u8>> {
    let width = columns * MAP_SIDE;
    let mut pictures = Vec::with_capacity(columns * rows);
    for my in 0..rows {
        for mx in 0..columns {
            let mut picture = Vec::with_capacity(PICTURE);
            for y in 0..MAP_SIDE {
                let from = (my * MAP_SIDE + y) * width + mx * MAP_SIDE;
                picture.extend_from_slice(&colors[from..from + MAP_SIDE]);
            }
            pictures.push(picture);
        }
    }
    pictures
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
        out
    }

    fn solid(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        pixel.repeat((width * height) as usize)
    }

    #[test]
    fn a_palette_colour_maps_to_itself_or_one_beside_it() {
        // The table is 5 bits a channel: a colour may land on a neighbour as close.
        for packed in palette() {
            let rgb = packed_rgb(packed);
            let got = packed_rgb(nearest(rgb));
            let off = (0..3).map(|i| (got[i] - rgb[i]).abs()).max().unwrap_or(0);
            assert!(off <= 12, "colour {packed}: {rgb:?} became {got:?}");
        }
    }

    #[test]
    fn a_square_image_fills_a_square_wall_and_its_colour_is_kept() {
        let white = png(4, 4, &solid(4, 4, [255, 255, 255, 255]));
        let frames = render(&white, 2, 2).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].delay_ms, 0);
        assert_eq!(frames[0].pictures.len(), 4);
        // Snow (id 8) at full shade is pure white.
        assert!(
            frames[0]
                .pictures
                .iter()
                .all(|p| p.iter().all(|&c| c == 8 * 4 + 2))
        );
    }

    #[test]
    fn a_wide_image_is_centred_between_transparent_bands() {
        let red = png(2, 1, &solid(2, 1, [255, 0, 0, 255]));
        let frames = render(&red, 1, 1).unwrap();
        let picture = &frames[0].pictures[0];
        assert_eq!(picture[0], 0, "the top band is transparent");
        assert_eq!(picture[127 * 128], 0, "the bottom band is transparent");
        assert_ne!(picture[64 * 128 + 64], 0, "the middle shows the image");
    }

    #[test]
    fn transparent_pixels_stay_transparent() {
        let clear = png(8, 8, &solid(8, 8, [10, 200, 30, 0]));
        let frames = render(&clear, 1, 1).unwrap();
        assert!(frames[0].pictures[0].iter().all(|&c| c == 0));
    }

    #[test]
    fn a_gif_keeps_its_frames_and_their_times() {
        let mut bytes = Vec::new();
        {
            let palette = [255, 0, 0, 0, 0, 255];
            let mut encoder = gif::Encoder::new(&mut bytes, 2, 2, &palette).unwrap();
            for (index, delay) in [(0u8, 20u16), (1, 1)] {
                let mut frame = gif::Frame {
                    width: 2,
                    height: 2,
                    delay,
                    ..gif::Frame::default()
                };
                frame.buffer = std::borrow::Cow::Owned(vec![index; 4]);
                encoder.write_frame(&frame).unwrap();
            }
        }
        let frames = render(&bytes, 1, 1).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].delay_ms, 200);
        assert_eq!(
            frames[1].delay_ms, GIF_DEFAULT_DELAY_MS,
            "a 10 ms frame shows for 100"
        );
        assert_ne!(frames[0].pictures[0][0], frames[1].pictures[0][0]);
    }

    #[test]
    fn a_long_animation_keeps_every_other_frame_for_twice_as_long() {
        let mut frames = Frames::new(4, 4);
        let pixel = solid(1, 1, [0, 0, 0, 255]);
        for _ in 0..40 {
            frames.push(1, 1, &pixel, 50);
        }
        let maps = 16;
        assert!(frames.frames.len() * maps * PICTURE <= MAX_RESULT);
        let total: u32 = frames.frames.iter().map(|f| f.delay_ms).sum();
        assert_eq!(total, 40 * 50, "the animation still lasts as long");
        assert!(frames.frames.len() < 40);
    }

    #[test]
    fn bad_spans_addresses_and_files_are_refused() {
        assert!(check_span(0, 1).is_err());
        assert!(check_span(17, 1).is_err());
        assert!(check_span(9, 8).is_err());
        assert!(check_span(8, 8).is_ok());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("ftp://example.com/a.png").is_err());
        assert!(check_url("HTTPS://example.com/a.png").is_ok());
        assert!(render(b"hello", 1, 1).is_err());
    }

    #[test]
    fn only_public_addresses_are_fetched() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["1.1.1.1", "140.82.112.3", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        assert!(public_addresses("localhost:80").is_err());
    }

    #[test]
    fn pictures_and_dimensions_are_checked() {
        assert!(picture_ok(&[0; PICTURE]).is_ok());
        assert!(picture_ok(&[0; 10]).is_err());
        assert!(picture_ok(&[255; PICTURE]).is_err());
        assert!(dimension_ok("minecraft:overworld"));
        assert!(!dimension_ok("Overworld"));
    }
}
