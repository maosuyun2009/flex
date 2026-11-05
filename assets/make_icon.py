"""Draw the flex mark and write assets/flex.png and assets/flex.ico.

A dark rounded tile, one pane outlined in cool grey, and an amber block sitting
on its corner: the split layout with the accent the UI already uses.
"""

from PIL import Image, ImageDraw

SIZE = 1024  # drawn large, then scaled down for each icon size
INK = (20, 22, 28, 255)
EDGE = (38, 43, 54, 255)
PANE = (154, 163, 178, 255)
AMBER = (240, 180, 41, 255)


def draw() -> Image.Image:
    image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    pen = ImageDraw.Draw(image)
    radius = int(SIZE * 0.22)
    pen.rounded_rectangle([0, 0, SIZE - 1, SIZE - 1], radius=radius, fill=INK)
    pen.rounded_rectangle(
        [0, 0, SIZE - 1, SIZE - 1], radius=radius, outline=EDGE, width=int(SIZE * 0.012)
    )
    # Two panes: one outlined, the focused one filled with the accent.
    stroke = int(SIZE * 0.062)
    radius = int(SIZE * 0.075)
    pen.rounded_rectangle(
        [SIZE * 0.11, SIZE * 0.20, SIZE * 0.47, SIZE * 0.80],
        radius=radius,
        outline=PANE,
        width=stroke,
    )
    pen.rounded_rectangle(
        [SIZE * 0.53, SIZE * 0.20, SIZE * 0.89, SIZE * 0.80],
        radius=radius,
        fill=AMBER,
    )
    return image


def main() -> None:
    mark = draw()
    mark.resize((512, 512), Image.LANCZOS).save("assets/flex.png")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [mark.resize((s, s), Image.LANCZOS) for s in sizes]
    frames[-1].save("assets/flex.ico", sizes=[(s, s) for s in sizes], append_images=frames[:-1])
    print("wrote assets/flex.png and assets/flex.ico")


if __name__ == "__main__":
    main()
