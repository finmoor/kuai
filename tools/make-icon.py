"""画应用图标，输出 crates/fr-app/assets/icon.ico（16 到 256 像素几种大小）。

图案是自己画的：深色圆角方块上一小段提交图，主线三个提交，右边分出一条线再连到服务器那头的一个点。
改了图案重新运行：python tools/make-icon.py（要装 Pillow）。
"""
from pathlib import Path

from PIL import Image, ImageDraw

SIZE = 1024  # 先画大图再缩小，边缘平滑
BACKGROUND = (24, 32, 48, 255)
MAIN = (96, 165, 250, 255)  # 主线：蓝
BRANCH = (52, 211, 153, 255)  # 分出去的线：绿
DOT_FILL = (24, 32, 48, 255)


def draw() -> Image.Image:
    image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    d = ImageDraw.Draw(image)
    d.rounded_rectangle((48, 48, SIZE - 48, SIZE - 48), radius=200, fill=BACKGROUND)

    width = 72
    left, right = 370, 680
    top, middle, bottom = 250, 512, 774

    # 主线
    d.line((left, top, left, bottom), fill=MAIN, width=width)
    # 分支：从下面的提交拐出去，到右上的提交
    d.line((left, bottom, left, 690), fill=MAIN, width=width)
    d.arc((left, 520, left + 2 * (right - left), 860), start=180, end=270, fill=BRANCH, width=width)
    d.line((right, 520, right, 380), fill=BRANCH, width=width)

    def dot(x: int, y: int, color) -> None:
        radius, hole = 92, 44
        d.ellipse((x - radius, y - radius, x + radius, y + radius), fill=color)
        d.ellipse((x - hole, y - hole, x + hole, y + hole), fill=DOT_FILL)

    for y in (top, middle, bottom):
        dot(left, y, MAIN)
    dot(right, 350, BRANCH)
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
