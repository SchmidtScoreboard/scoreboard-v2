#!/usr/bin/env python3
"""Compose App Store / Play Store promo images from raw app captures.

    python3 design/store/compose.py

Inputs (all under design/store/raw/):
  ios/*.png      iPhone 17 Pro Max captures, 1320x2868 (simctl / XCUIScreen)
  ipad/*.png     iPad Pro 13-inch captures, 2064x2752
  android/*.png  Pixel 9 emulator captures, 1080x2424 (adb screencap)
  board/*.png    64x32 server frames (from `server --preview`), drawn as LEDs

Outputs (design/store/out/), every one RGB with no alpha as both stores want:
  app-store-6.9/   1320x2868  iPhone 6.9" (Apple scales it down for smaller phones)
  app-store-13/    2064x2752  iPad 13"
  play-phone/      1440x2560  Play phone screenshots (9:16)
  play-feature.png 1024x500   Play feature graphic
"""
import os
from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
RAW = os.path.join(HERE, 'raw')
OUT = os.path.join(HERE, 'out')

# SF Pro's licence covers mock-ups for Apple platforms only, so the Play
# images use Avenir Next (a macOS system font) instead.
SF_BOLD = '/Library/Fonts/SF-Pro-Display-Bold.otf'
SF_TEXT = '/Library/Fonts/SF-Pro-Display-Regular.otf'
AVENIR = '/System/Library/Fonts/Avenir Next.ttc'
AV_BOLD, AV_MEDIUM = 0, 5

BG_TOP, BG_BOTTOM = (13, 17, 30), (4, 5, 9)
TITLE, SUBTITLE = (255, 255, 255), (160, 170, 190)

# One list drives every platform. `shot` names a capture per platform.
SLIDES = [
    dict(key='01-hero', hero=True, accent=(255, 184, 28),
         title='Live scores,\non your wall.',
         sub='A real LED scoreboard that follows your teams.',
         shot=dict(ios='dashboard', ipad='dashboard', android='dashboard')),
    dict(key='02-teams', accent=(10, 132, 255),
         title='Follow your teams.',
         sub='Hockey, basketball, baseball, football, soccer, golf and F1.',
         shot=dict(ios='teams', ipad='teams', android='teams')),
    dict(key='03-situations', accent=(48, 209, 88),
         title='Every situation,\nat a glance.',
         sub='Downs and field position. Bases and the count.',
         shot=dict(ios='situations', ipad='situations', android='situations')),
    dict(key='04-leaderboards', accent=(255, 69, 58),
         title='Leaderboards, too.',
         sub='Golf majors and Formula 1 on the same panel.',
         shot=dict(ios='leaderboards', ipad='leaderboards', android='leaderboards')),
    dict(key='05-make-it-yours', accent=(191, 90, 242),
         title='Make it yours.',
         sub='Big games, spoilers, team names — every choice previewed on the panel.',
         shot=dict(ios='league', ipad='league', android='league')),
    dict(key='06-screens', accent=(100, 210, 255),
         title='Clock and weather\nwhen nothing’s on.',
         sub='Quiet hours put the board to sleep overnight.',
         shot=dict(ios='screens', ipad='screens', android='screens')),
]

# Per-target geometry. `screen` is the device kind drawn around the capture.
TARGETS = {
    'app-store-6.9': dict(size=(1320, 2868), raw='ios', screen='iphone', font='sf',
                          title=0.088, sub=0.040, top=0.075, device_w=0.80),
    'app-store-13': dict(size=(2064, 2752), raw='ipad', screen='ipad', font='sf',
                         title=0.064, sub=0.029, top=0.065, device_w=0.80),
    'play-phone': dict(size=(1440, 2560), raw='android', screen='pixel', font='avenir',
                       title=0.080, sub=0.036, top=0.065, device_w=0.72),
}


def font(kind, weight, px):
    if kind == 'sf':
        return ImageFont.truetype(SF_BOLD if weight == 'bold' else SF_TEXT, px)
    return ImageFont.truetype(AVENIR, px, index=AV_BOLD if weight == 'bold' else AV_MEDIUM)


# ---------------------------------------------------------------- background

def background(size, accent, glow_center, strength=100):
    W, H = size
    bg = Image.new('RGB', (1, 256))
    for y in range(256):
        t = y / 255
        bg.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOTTOM)))
    bg = bg.resize(size, Image.BICUBIC).convert('RGBA')

    # A faint LED-matrix texture: the board's own grid, unlit.
    pitch = W / 40
    dots = Image.new('L', size, 0)
    d = ImageDraw.Draw(dots)
    r = pitch * 0.13
    y = pitch / 2
    while y < H:
        x = pitch / 2
        while x < W:
            d.ellipse((x - r, y - r, x + r, y + r), fill=255)
            x += pitch
        y += pitch
    fade = Image.linear_gradient('L').resize(size).point(lambda v: 26 - v * 18 // 255)
    bg.alpha_composite(Image.merge('RGBA', [Image.new('L', size, 255)] * 3 + [ImageChops.multiply(dots, fade)]))

    # Accent glow behind the device.
    gx, gy = glow_center
    glow = Image.new('L', size, 0)
    rx, ry = W * 0.55, H * 0.28
    ImageDraw.Draw(glow).ellipse((gx - rx, gy - ry, gx + rx, gy + ry), fill=strength)
    glow = glow.filter(ImageFilter.GaussianBlur(W * 0.16))
    bg.alpha_composite(Image.merge('RGBA', [Image.new('L', size, c) for c in accent] + [glow]))
    return bg


# ---------------------------------------------------------------- text

def wrap(text, f, max_w):
    lines = []
    for para in text.split('\n'):
        words, line = para.split(' '), ''
        for w in words:
            trial = (line + ' ' + w).strip()
            if f.getlength(trial) <= max_w or not line:
                line = trial
            else:
                lines.append(line)
                line = w
        lines.append(line)
    return lines


def balanced(text, f, max_w):
    """Wrap into as few lines as `max_w` allows, then narrow the width until
    one more step would add a line, so no line ends up a lone word."""
    n = len(wrap(text, f, max_w))
    lo, hi = max_w * 0.4, max_w
    for _ in range(20):
        mid = (lo + hi) / 2
        if len(wrap(text, f, mid)) > n:
            lo = mid
        else:
            hi = mid
    return wrap(text, f, hi)


def draw_text(img, slide, t, top):
    """Centered title + subtitle; returns the y just below them."""
    W, _ = img.size
    d = ImageDraw.Draw(img)
    tf = font(t['font'], 'bold', round(W * t['title']))
    sf = font(t['font'], 'regular', round(W * t['sub']))
    y = top
    for line in wrap(slide['title'], tf, W * 0.9):
        d.text((W / 2, y), line, font=tf, fill=TITLE, anchor='ma')
        y += tf.size * 1.12
    y += tf.size * 0.32
    for line in balanced(slide['sub'], sf, W * 0.80):
        d.text((W / 2, y), line, font=sf, fill=SUBTITLE, anchor='ma')
        y += sf.size * 1.32
    return y


# ---------------------------------------------------------------- devices

SS = 3  # supersampling for anti-aliased masks


def rrect_mask(size, radius):
    w, h = size
    m = Image.new('L', (w * SS, h * SS), 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, w * SS - 1, h * SS - 1), radius * SS, fill=255)
    return m.resize(size, Image.LANCZOS)


def device(shot, screen_w, kind):
    """The capture in a simple flat device body; returns RGBA."""
    sw = round(screen_w)
    sh = round(shot.height * sw / shot.width)
    screen = shot.convert('RGB').resize((sw, sh), Image.LANCZOS)
    spec = {
        'iphone': dict(bezel=0.030, radius=0.135),
        'pixel': dict(bezel=0.032, radius=0.100),
        'ipad': dict(bezel=0.026, radius=0.026),
    }[kind]
    bz = round(sw * spec['bezel'])
    r = sw * spec['radius']
    btn = round(sw * 0.008)  # side buttons stand proud of the body
    W, H = sw + 2 * bz + 2 * btn, sh + 2 * bz
    out = Image.new('RGBA', (W, H), (0, 0, 0, 0))

    def body_layer(rect, radius, colour):
        x0, y0, x1, y1 = rect
        m = rrect_mask((x1 - x0, y1 - y0), radius)
        out.paste(Image.new('RGBA', m.size, colour), (x0, y0), m)

    # Side buttons first, so the body sits over their inner edges.
    def side(x, y_frac, h_frac):
        y0 = round(H * y_frac)
        body_layer((x, y0, x + btn * 3, y0 + round(H * h_frac)), btn, (44, 46, 52, 255))
    if kind == 'iphone':
        side(0, 0.17, 0.045); side(0, 0.25, 0.075); side(0, 0.34, 0.075); side(W - 3 * btn, 0.26, 0.11)
    elif kind == 'pixel':
        side(W - 3 * btn, 0.20, 0.07); side(W - 3 * btn, 0.31, 0.12)
    else:
        side(W - 3 * btn, 0.06, 0.05); side(W - 3 * btn, 0.12, 0.05)

    # Body: a lighter rim, then the black bezel inside it.
    body_layer((btn, 0, W - btn, H), r + bz, (58, 61, 68, 255))
    rim = max(2, round(sw * 0.004))
    body_layer((btn + rim, rim, W - btn - rim, H - rim), r + bz - rim, (10, 10, 12, 255))

    out.paste(screen, (btn + bz, bz), rrect_mask((sw, sh), r))
    d = ImageDraw.Draw(out)
    if kind == 'iphone':
        iw, ih, it = sw * 126 / 440, sw * 37 / 440, sw * 11 / 440
        cx = btn + bz + sw / 2
        d.rounded_rectangle((cx - iw / 2, bz + it, cx + iw / 2, bz + it + ih), ih / 2, fill=(0, 0, 0, 255))
    elif kind == 'pixel':
        cr, cy = sw * 0.019, bz + sw * 0.045
        cx = btn + bz + sw / 2
        d.ellipse((cx - cr, cy - cr, cx + cr, cy + cr), fill=(0, 0, 0, 255))
    return out


def shadow_of(img, blur, alpha=150):
    a = img.getchannel('A').point(lambda v: v * alpha // 255)
    pad = round(blur * 3)
    canvas = Image.new('L', (img.width + 2 * pad, img.height + 2 * pad), 0)
    canvas.paste(a, (pad, pad))
    canvas = canvas.filter(ImageFilter.GaussianBlur(blur))
    return Image.merge('RGBA', [Image.new('L', canvas.size, 0)] * 3 + [canvas]), pad


# ---------------------------------------------------------------- LED board

def load_frame(name):
    """A 64x32 frame. `server --preview` writes them at 12x; sample centres."""
    im = Image.open(os.path.join(RAW, 'board', name + '.png')).convert('RGB')
    if im.size == (64, 32):
        return im
    k = im.width // 64
    small = Image.new('RGB', (64, 32))
    for y in range(32):
        for x in range(64):
            small.putpixel((x, y), im.getpixel((x * k + k // 2, y * k + k // 2)))
    return small


def led_board(frame, width):
    """The physical board — black face, square LEDs, the dial on the left
    edge a third of the way down (as in design/icon) — plus a glow halo.
    Returns (board RGBA, halo RGBA, halo offset)."""
    cols, rows = frame.size
    p = width / (cols + 2 * 1.4 + cols * 0.05)
    bez, dial_w = 1.4 * p, cols * 0.05 * p
    pw, ph = cols * p, rows * p
    W, H = round(dial_w + pw + 2 * bez), round(ph + 2 * bez)
    k = SS
    big = Image.new('RGBA', (W * k, H * k), (0, 0, 0, 0))
    d = ImageDraw.Draw(big)
    face_x = dial_w
    dial_h = (ph + 2 * bez) * 0.40
    dial_y = (ph + 2 * bez) * 0.33 - dial_h / 2
    d.rounded_rectangle((0, dial_y * k, (dial_w + bez) * k, (dial_y + dial_h) * k), dial_w * 0.45 * k, fill=(42, 43, 48, 255))
    d.rounded_rectangle((face_x * k, 0, W * k - 1, H * k - 1), 1.3 * p * k, fill=(11, 11, 14, 255))
    lit = Image.new('RGB', (W * k, H * k), (0, 0, 0))
    ld = ImageDraw.Draw(lit)
    dot = p * 0.84
    off = (p - dot) / 2
    for y in range(rows):
        for x in range(cols):
            c = frame.getpixel((x, y))
            on = max(c) > 24
            x0 = (face_x + bez + x * p + off) * k
            y0 = (bez + y * p + off) * k
            box = (x0, y0, x0 + dot * k, y0 + dot * k)
            d.rounded_rectangle(box, dot * 0.2 * k, fill=(c + (255,)) if on else (26, 27, 31, 255))
            if on:
                ld.rectangle((x0 - off * k, y0 - off * k, x0 + (dot + off) * k, y0 + (dot + off) * k), fill=c)
    board = big.resize((W, H), Image.LANCZOS)
    lit = lit.resize((W, H), Image.BILINEAR)

    # Bloom on the LEDs themselves, then a wide halo for the wall behind.
    bloom = lit.filter(ImageFilter.GaussianBlur(p * 0.45)).point(lambda v: v * 0.35)
    rgb = ImageChops.screen(board.convert('RGB'), bloom)
    board = Image.merge('RGBA', rgb.split() + (board.getchannel('A'),))
    pad = round(p * 14)
    halo = Image.new('RGB', (W + 2 * pad, H + 2 * pad), (0, 0, 0))
    halo.paste(lit, (pad, pad))
    halo = halo.filter(ImageFilter.GaussianBlur(p * 5))
    halo_a = halo.convert('L').point(lambda v: min(255, v * 1.3))
    halo = Image.merge('RGBA', halo.split() + (halo_a,))
    return board, halo, pad


# ---------------------------------------------------------------- slides

def raw_shot(platform, name):
    path = os.path.join(RAW, platform, name + '.png')
    return Image.open(path) if os.path.exists(path) else None


def compose_slide(slide, t):
    W, H = t['size']
    shot = raw_shot(t['raw'], slide['shot'][t['raw']])
    if shot is None:
        return None
    screen_w = W * t['device_w']
    dev = device(shot, screen_w, t['screen'])

    if slide.get('hero'):
        img = background((W, H), slide['accent'], (W / 2, H * 0.55), 70)
        y = draw_text(img, slide, t, H * t['top'])
        board, halo, pad = led_board(load_frame('nhl_active'), W * (0.86 if W < 1800 else 0.70))
        bx, by = round((W - board.width) / 2), round(y + H * 0.035)
        img.alpha_composite(halo, (bx - pad, by - pad))
        sh, spad = shadow_of(board, W * 0.02, 200)
        img.alpha_composite(sh, (bx - spad, by - spad + round(W * 0.015)))
        img.alpha_composite(board, (bx, by))
        dev_top = by + board.height + H * 0.045
    else:
        img = background((W, H), slide['accent'], (W / 2, H * 0.62))
        y = draw_text(img, slide, t, H * t['top'])
        dev_top = y + H * 0.035

    dx = round((W - dev.width) / 2)
    sh, spad = shadow_of(dev, W * 0.03, 170)
    img.alpha_composite(sh, (dx - spad, round(dev_top) - spad + round(W * 0.02)))
    img.alpha_composite(dev, (dx, round(dev_top)))
    return img.convert('RGB')


def feature_graphic():
    W, H = 1024, 500
    img = background((W, H), (255, 184, 28), (W * 0.74, H * 0.5))
    board, halo, pad = led_board(load_frame('nhl_active'), 430)
    bx, by = W - board.width - 52, round((H - board.height) / 2)
    img.alpha_composite(halo, (bx - pad, by - pad))
    sh, spad = shadow_of(board, 14, 200)
    img.alpha_composite(sh, (bx - spad, by - spad + 10))
    img.alpha_composite(board, (bx, by))
    d = ImageDraw.Draw(img)
    max_w = bx - 56 - 36
    size = 64
    while font('avenir', 'bold', size).getlength('OpenScoreboard') > max_w:
        size -= 1
    tf = font('avenir', 'bold', size)
    sf = font('avenir', 'regular', round(size * 0.44))
    lines = wrap('Live scores on a real LED scoreboard.', sf, max_w)
    block = tf.size * 1.15 + len(lines) * sf.size * 1.35
    y = (H - block) / 2
    d.text((56, y), 'OpenScoreboard', font=tf, fill=TITLE)
    y += tf.size * 1.25
    for line in lines:
        d.text((56, y), line, font=sf, fill=SUBTITLE)
        y += sf.size * 1.35
    return img.convert('RGB')


def main():
    for name, t in TARGETS.items():
        os.makedirs(os.path.join(OUT, name), exist_ok=True)
        for slide in SLIDES:
            img = compose_slide(slide, t)
            if img is None:
                print(f'skip {name}/{slide["key"]} (no raw capture)')
                continue
            assert img.size == t['size']
            path = os.path.join(OUT, name, slide['key'] + '.png')
            img.save(path, optimize=True)
            print('wrote', os.path.relpath(path, HERE))
    path = os.path.join(OUT, 'play-feature.png')
    feature_graphic().save(path, optimize=True)
    print('wrote', os.path.relpath(path, HERE))


if __name__ == '__main__':
    main()
