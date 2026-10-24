"""Owned fixture data only; preserve previous outputs instead of deleting them."""
from pathlib import Path
import re
import shutil

BASE = Path('/tmp/orca-cu-baseline')
DRAFT = 'teh quick fox\nsecond baseline line\nthird baseline line\n'
HEADING = 'Orca Copper Otter 2026'
BUTTON = 'Orca button confirmed: blue lantern'


def make_pdf(path):
    objects = [b'<< /Type /Catalog /Pages 2 0 R >>',
               b'<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>']
    for n in range(2):
        objects.append(f'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents {6+n} 0 R >>'.encode())
    objects.append(b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>')
    for n in range(1, 3):
        stream = f'BT /F1 24 Tf 60 700 Td (Orca baseline - page {n} of 2) Tj ET\n'.encode()
        objects.append(f'<< /Length {len(stream)} >>\nstream\n'.encode() + stream + b'endstream')
    data = bytearray(b'%PDF-1.4\n'); offsets = [0]
    for i, obj in enumerate(objects, 1):
        offsets.append(len(data)); data.extend(f'{i} 0 obj\n'.encode() + obj + b'\nendobj\n')
    xref = len(data)
    data.extend(f'xref\n0 {len(offsets)}\n0000000000 65535 f \n'.encode())
    for offset in offsets[1:]:
        data.extend(f'{offset:010} 00000 n \n'.encode())
    data.extend(f'trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n'.encode())
    path.write_bytes(data)


def pdf_page_count(path):
    """Structural count of our uncompressed fixture; PDFKit also checks at run time."""
    data = path.read_bytes()
    return len(re.findall(rb'/Type\s*/Page\b', data))


def prepare(base=BASE):
    for name in ('fixtures/files120', 'out', 'ws', 'home'):
        (base / name).mkdir(parents=True, exist_ok=True)
    fixtures = base / 'fixtures'
    (fixtures / 'page.html').write_text(f'''<!doctype html><meta charset="utf-8"><title>Orca Baseline Fixture</title>
<style>body{{font:24px system-ui;padding:50px}}button{{font:24px system-ui;padding:16px}}</style>
<h1>{HEADING}</h1><button onclick="document.getElementById('result').textContent='{BUTTON}'">Press me</button><p id="result"></p>''')
    (fixtures / 'form.html').write_text('''<!doctype html><meta charset="utf-8"><title>Orca Form Fixture</title>
<style>body{font:24px system-ui;padding:50px}input,button{font:24px system-ui}</style><h1>Orca form</h1>
<form method="POST" action="/form.html"><label>Name <input name="name"></label><p><label><input type="checkbox" name="agree" value="yes">Agree</label></p><button>Submit</button></form>''')
    for i in range(1, 121):
        (fixtures / 'files120' / f'file-{i:03}.txt').write_text(f'Orca baseline fixture {i}\n')
    (fixtures / 'draft.txt').write_text(DRAFT)
    make_pdf(fixtures / 'two-pages.pdf')
    if pdf_page_count(fixtures / 'two-pages.pdf') != 2:
        raise RuntimeError('PDF fixture must contain exactly two pages')


def archive_previous(base, destination):
    for name in ('home', 'ws', 'fixtures', 'out', 'state'):
        path = base / name
        if path.is_symlink():
            raise RuntimeError(f'Refusing symlink: {path}')
        if path.exists():
            destination.mkdir(parents=True, exist_ok=True)
            shutil.move(str(path), str(destination / name))


def reset_outputs(base, saved):
    saved.mkdir(parents=True, exist_ok=True)
    for p in list((base / 'out').iterdir()):
        shutil.move(str(p), str(saved / p.name))
    p = base / 'ws/counter.html'
    if p.exists():
        shutil.move(str(p), str(saved / 'counter.html'))
    (base / 'fixtures/draft.txt').write_text(DRAFT)
