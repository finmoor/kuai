"""画应用图标，输出 crates/fr-app/assets/icon.ico（16 到 256 像素几种大小）。

图案是自己画的：深色圆角方块上一个 K。竖线是提交历史（上下各一个提交），一双筷子从右边伸过来
夹住中间那个提交。产品叫 Kuai（筷子），缩到 16 像素还认得出是 K。
改了图案重新运行：python tools/make-icon.py（要装 Pillow）。
"""
import math
from pathlib import Path

from PIL import Image, ImageDraw

SIZE = 1024  # 先画大图再缩小，边缘平滑
BACKGROUND = (24, 32, 48, 255)
LINE = (96, 165, 250, 255)  # 提交历史：蓝
PICKED = (251, 191, 36, 255)  # 被夹住的那个提交：黄
STICK_LIGHT = (241, 226, 196, 255)  # 两根筷子一浅一深
STICK_DARK = (214, 170, 110, 255)


def stick(d: ImageDraw.ImageDraw, start, end, w_start: int, w_end: int, color) -> None:
    """一根筷子：从粗的一头到细的一头，两头是圆的。"""
    (x1, y1), (x2, y2) = start, end
    angle = math.atan2(y2 - y1, x2 - x1)
    nx, ny = -math.sin(angle), math.cos(angle)
    d.polygon(
        [
            (x1 + nx * w_start, y1 + ny * w_start),
            (x2 + nx * w_end, y2 + ny * w_end),
            (x2 - nx * w_end, y2 - ny * w_end),
            (x1 - nx * w_start, y1 - ny * w_start),
        ],
        fill=color,
    )
    d.ellipse((x1 - w_start, y1 - w_start, x1 + w_start, y1 + w_start), fill=color)
    d.ellipse((x2 - w_end, y2 - w_end, x2 + w_end, y2 + w_end), fill=color)


def draw() -> Image.Image:
    image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    d = ImageDraw.Draw(image)
    d.rounded_rectangle((48, 48, SIZE - 48, SIZE - 48), radius=200, fill=BACKGROUND)

    x, top, middle, bottom = 330, 236, 512, 788

    def dot(cx: int, cy: int, color, radius: int = 92, hole: int = 44) -> None:
        d.ellipse((cx - radius, cy - radius, cx + radius, cy + radius), fill=color)
        d.ellipse((cx - hole, cy - hole, cx + hole, cy + hole), fill=BACKGROUND)

    # 提交历史
    d.line((x, top, x, bottom), fill=LINE, width=72)
    # 一双筷子，细的一头夹着中间的提交
    stick(d, (742, 214), (x + 70, middle - 54), 46, 26, STICK_LIGHT)
    stick(d, (742, 810), (x + 70, middle + 54), 46, 26, STICK_DARK)
    dot(x, top, LINE)
    dot(x, bottom, LINE)
    dot(x, middle, PICKED, radius=100, hole=48)
    return image


def main() -> None:
    out = Path(__file__).resolve().parent.parent / "crates" / "fr-app" / "assets" / "icon.ico"
    out.parent.mkdir(parents=True, exist_ok=True)
    image = draw()
    sizes = [(s, s) for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)]
    image.save(out, format="ICO", sizes=sizes)
    image.resize((256, 256), Image.LANCZOS).save(out.with_suffix(".png"))
    print(f"写好了：{out}")


if __name__ == "__main__":
    main()
