"""画安装向导最后一页左边的竖条图：和应用图标同一个底色，中间是图标和「Kuai」。

Inno Setup 现代样式的这张图按 100% 缩放是 164×314，向导默认放大到 120%；
按几种屏幕缩放各出一张，Inno 挑最接近的用（kuai.iss 的 WizardImageFile）。

用法：python tools/installer/make-wizard-image.py（要 Pillow 和 Windows 自带的 Segoe UI）
"""
import pathlib

from PIL import Image, ImageDraw, ImageFont

HERE = pathlib.Path(__file__).resolve().parent
ICON = HERE.parent.parent / 'crates' / 'fr-app' / 'assets' / 'icon.png'
FONT = pathlib.Path('C:/Windows/Fonts/seguisb.ttf')
BASE = (164, 314)
SCALES = (125, 150, 200, 250, 300)
TOP = (30, 39, 58)      # 比图标底色 #182030 稍亮，往下渐暗到图标底色
BOTTOM = (24, 32, 48)
TEXT = (232, 235, 241)


def draw(scale):
    w, h = (round(v * scale / 100) for v in BASE)
    image = Image.new('RGB', (w, h))
    pixels = ImageDraw.Draw(image)
    for y in range(h):
        t = y / (h - 1)
        pixels.line([(0, y), (w, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)))
    icon = Image.open(ICON).convert('RGBA')
    size = round(w * 0.56)
    icon = icon.resize((size, size), Image.LANCZOS)
    top = round(h * 0.30)
    image.paste(icon, ((w - size) // 2, top), icon)
    font = ImageFont.truetype(str(FONT), round(w * 0.16))
    text = 'Kuai'
    box = pixels.textbbox((0, 0), text, font=font)
    x = (w - (box[2] - box[0])) // 2 - box[0]
    pixels.text((x, top + size + round(w * 0.06)), text, font=font, fill=TEXT)
    out = HERE / f'wizard-{scale}.png'
    image.save(out, optimize=True)
    print(out.name, image.size)


for scale in SCALES:
    draw(scale)
