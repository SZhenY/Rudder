from PIL import Image
import os

BASE = os.path.dirname(os.path.abspath(__file__))
src = os.path.join(BASE, "rudder_icon_1024.png")

img = Image.open(src).convert("RGBA")
print(f"Source: {img.size[0]}x{img.size[1]}")

# icon.png → 256x256 (general / winresource fallback)
img.resize((256, 256), Image.LANCZOS).save(os.path.join(BASE, "icon.png"), optimize=True)

# icon@512.png → 512x512 (macOS icns base / Linux AppDir)
img.resize((512, 512), Image.LANCZOS).save(os.path.join(BASE, "icon@512.png"), optimize=True)

# rudder.ico → 16, 32, 48, 256 (Windows Explorer / taskbar / Start Menu)
img.save(os.path.join(BASE, "rudder.ico"), format="ICO", sizes=[(16, 16), (32, 32), (48, 48), (256, 256)])

# Report
for name in ["icon.png", "icon@512.png", "rudder_icon_1024.png", "rudder.ico"]:
    path = os.path.join(BASE, name)
    kb = os.path.getsize(path) / 1024
    s = Image.open(path)
    desc = ""
    if name == "rudder.ico":
        desc = f" [sizes: 16 32 48 256]"
    print(f"  {name}: {s.size[0]}x{s.size[1]}  ({kb:.0f} KB){desc}")

print("\nDone. Each platform now gets the right resolution:")
print("  Windows:  rudder.ico  (16/32/48/256 embedded)")
print("  macOS:    icon@512.png → sips → .icns  (16-512px)")
print("  Linux:    icon@512.png → hicolor/512x512/rudder.png")
