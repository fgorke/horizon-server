# Packs the head catalog HeadDB reads (data.headsdb.com: manifest.json, its catalog index and
# catalog parts, unpacked from zstd) into catalog/heads.bin, the format src/db.rs reads.
# python3 tools/pack.py <index.json> <catalog-part.json> catalog/heads.bin
import json, struct, sys
idx = json.load(open(sys.argv[1]))
data = json.load(open(sys.argv[2]))
cats = idx['categories']; tags = idx['tags']
cat_i = {c['id']: i for i, c in enumerate(cats)}
tag_i = {t['id']: i for i, t in enumerate(tags)}
out = bytearray(b'HDB1')
def s8(b):
    b = b.encode()[:255]; out.append(len(b)); out.extend(b)
out += struct.pack('<H', len(cats))
for c in cats: s8(c['id']); s8(c['name'])
out += struct.pack('<H', len(tags))
for t in tags: s8(t['name'])
heads = data['heads']
skipped = 0
body = bytearray(); n = 0
for h in heads:
    tex = h['texture']
    if any(ch not in '0123456789abcdef' for ch in tex) or len(tex) > 64 or h['category'] not in cat_i:
        skipped += 1; continue
    name = h['name'].strip()
    if not name: skipped += 1; continue
    body.append(cat_i[h['category']])
    body.append(len(tex))
    body.extend(bytes.fromhex(tex if len(tex) % 2 == 0 else '0' + tex))
    nb = name.encode()[:255]; body.append(len(nb)); body.extend(nb)
    ts = [tag_i[t] for t in h.get('tags', []) if t in tag_i][:255]
    body.append(len(ts))
    for t in ts: body.extend(struct.pack('<H', t))
    n += 1
out += struct.pack('<I', n) + body
open(sys.argv[3], 'wb').write(out)
print('heads', n, 'skipped', skipped, 'bytes', len(out), 'cats', len(cats), 'tags', len(tags))
