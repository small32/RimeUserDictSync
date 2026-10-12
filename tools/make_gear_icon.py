"""把用户给的齿轮 PNG 转成 **白底、不透明** 的多尺寸 .ico。

源图是"白底 + 黑色齿轮 + 很淡的投影"。这里不做透明通道：按用户要求保留白底，
只是把接近白的那部分统一压成纯白，免得缩到 16x16 时背景发灰。
"""

import sys

from PIL import Image

SOURCE = sys.argv[1]
TARGET = sys.argv[2]

image = Image.open(SOURCE).convert("RGBA")
print(f"源图: {image.size[0]}x{image.size[1]} mode={image.mode}")

# 先压到白底上，丢掉源图自带的 alpha
flat = Image.new("RGB", image.size, (255, 255, 255))
flat.paste(image, (0, 0), image)
pixels = list(flat.getdata())
print(f"最深的前景色: {min(pixels, key=sum)}")

# 把接近白的压成纯白，其余保留，这样缩小时不会有一圈灰边
WHITE_CUTOFF = 235
cleaned = Image.new("RGB", image.size)
cleaned.putdata(
    [
        (255, 255, 255) if min(r, g, b) >= WHITE_CUTOFF else (r, g, b)
        for r, g, b in pixels
    ]
)

# 裁到齿轮的边界（相对纯白的差异），让图标尽量占满
mask = Image.new("L", image.size)
mask.putdata([0 if pixel == (255, 255, 255) else 255 for pixel in cleaned.getdata()])
bbox = mask.getbbox()
if bbox:
    cleaned = cleaned.crop(bbox)
    print(f"裁剪到内容: {cleaned.size[0]}x{cleaned.size[1]}")

# 补成正方形并留一点边距，否则 16x16 会被拉变形
side = max(cleaned.size)
margin = max(1, int(side * 0.06))
canvas = Image.new("RGB", (side + margin * 2, side + margin * 2), (255, 255, 255))
canvas.paste(
    cleaned,
    ((canvas.width - cleaned.width) // 2, (canvas.height - cleaned.height) // 2),
)

# Pillow 写 .ico 时会自己做 LANCZOS 缩放，基准图给到 256 才能生成 256x256 那一帧
if canvas.width < 256:
    canvas = canvas.resize((256, 256), Image.Resampling.LANCZOS)
print(f"画布: {canvas.size[0]}x{canvas.size[1]}（白底不透明）")

sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
canvas.save(TARGET, format="ICO", sizes=sizes)
print(f"已写出 {TARGET}")

# 预览：16 / 32 / 48 三档放大 4 倍拼一排，检查小尺寸下是否还认得出
preview = Image.new("RGB", (16 * 4 + 32 * 4 + 48 * 4 + 60, 48 * 4 + 20), (190, 190, 190))
x = 10
for size in (16, 32, 48):
    frame = Image.open(TARGET).ico.getimage((size, size)).convert("RGB")
    frame = frame.resize((size * 4, size * 4), Image.Resampling.NEAREST)
    preview.paste(frame, (x, 10))
    x += size * 4 + 20
preview.save(str(TARGET).replace(".ico", "-preview.png"))
print("预览已写出（16 / 32 / 48 各放大 4 倍）")
