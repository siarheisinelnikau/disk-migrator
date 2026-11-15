"""Inspect the real PE resources and imports without loading or executing the app."""
from pathlib import Path
import struct
import sys

root = Path(__file__).resolve().parents[1]
path = Path(sys.argv[1]) if len(sys.argv) > 1 else root / 'dist/DiskMigrator-v0.1.0.exe'
data = path.read_bytes()
u16 = lambda pos: struct.unpack_from('<H', data, pos)[0]
u32 = lambda pos: struct.unpack_from('<I', data, pos)[0]
pe = u32(60)
assert data[:2] == b'MZ' and data[pe:pe+4] == b'PE\0\0', 'Invalid PE file'
assert u16(pe+4) == 0x8664 and u16(pe+24) == 0x20b, 'Expected Windows x64 PE32+'
sections = []
start = pe + 24 + u16(pe+20)
for i in range(u16(pe+6)):
    pos = start+i*40
    sections.append((u32(pos+12), max(u32(pos+8), u32(pos+16)), u32(pos+20)))

def offset(rva):
    for address, size, raw in sections:
        if address <= rva < address+size:
            return raw+rva-address
    raise AssertionError(f'Invalid resource/import RVA {rva}')

base = offset(u32(pe+24+112+16))
resources = {}

def walk(relative, keys=()):
    pos = base+relative
    for i in range(u16(pos+12)+u16(pos+14)):
        name, child = struct.unpack_from('<II', data, pos+16+i*8)
        key = name if name & 0x80000000 == 0 else 'named'
        if child & 0x80000000:
            walk(child & 0x7fffffff, keys+(key,))
        else:
            rva, size = struct.unpack_from('<II', data, base+child)
            at = offset(rva)
            resources[keys+(key,)] = data[at:at+size]

walk(0)
original = (root/'assets/app.ico').read_bytes()
expected = {}
for i in range(struct.unpack_from('<H', original, 4)[0]):
    pos = 6+i*16
    w, h, _, _, _, _, size, at = struct.unpack_from('<BBBBHHII', original, pos)
    expected[(w or 256, h or 256)] = original[at:at+size]
groups = [value for keys, value in resources.items() if keys[0] == 14]
assert len(groups) == 1, 'Expected one application icon group'
group = groups[0]
assert struct.unpack_from('<H', group, 4)[0] == len(expected), 'Missing icon sizes'
for i in range(len(expected)):
    w, h, _, _, _, _, size, number = struct.unpack_from('<BBBBHHIH', group, 6+i*14)
    icon = next(value for keys, value in resources.items() if keys[0] == 3 and keys[1] == number)
    assert len(icon) == size and icon == expected[(w or 256, h or 256)], 'Icon data mismatch'
    alpha = icon[40+3:40+(w or 256)*(h or 256)*4:4]
    assert any(alpha) and any(a == 0 for a in alpha), 'Missing artwork or transparent margins'
assert any(b'asInvoker' in value for keys, value in resources.items() if keys[0] == 24), 'Missing unelevated manifest'
imports = []
directory = u32(pe+24+112+8)
if directory:
    pos = offset(directory)
    while any(data[pos:pos+20]):
        name = offset(u32(pos+12))
        imports.append(data[name:data.index(b'\0', name)].decode('ascii').lower())
        pos += 20
blocked = ('opengl', 'd3d', 'dxgi', 'vulkan', 'vcruntime', 'msvcp')
assert not any(any(word in dll for word in blocked) for dll in imports), imports
assert 'gdi32.dll' in imports, 'Expected the GDI presentation path'
print(f'PASS: x64 PE, {len(expected)} exact icon resources, asInvoker, GDI imports, static CRT.')
print('Runtime window/taskbar identity, idle/hidden rendering and hardware behavior remain untested.')
