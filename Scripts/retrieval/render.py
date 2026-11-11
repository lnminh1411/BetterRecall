# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Render synthetic 1280x720 screens of known content plus the query set."""
import json, random, sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
F = r"C:\Windows\Fonts"
def font(name, size): return ImageFont.truetype(F + "\\" + name, size)
mono, ui, uib = font("consola.ttf", 18), font("segoeui.ttf", 20), font("segoeuib.ttf", 24)
W, H = 1280, 720
random.seed(7)
def new(bg):
    im = Image.new("RGB", (W, H), bg); return im, ImageDraw.Draw(im)
def titlebar(d, text, dark=False):
    d.rectangle([0, 0, W, 36], fill=(32, 32, 32) if dark else (235, 235, 235))
    d.text((14, 6), text, font=ui, fill=(220, 220, 220) if dark else (30, 30, 30))
imgs = {}

im, d = new((30, 30, 30)); titlebar(d, "main.py - Visual Studio Code", True)
d.rectangle([0, 36, 220, H], fill=(37, 37, 38))
for i, f in enumerate(["main.py", "utils.py", "models.py", "README.md"]): d.text((20, 60 + i * 28), f, font=ui, fill=(200, 200, 200))
code = ["import numpy as np", "", "def train(model, data, epochs=10):", "    for epoch in range(epochs):", "        loss = model.step(data)",
        "        print(f'epoch {epoch}: {loss:.4f}')", "    return model", "", "class Model:", "    def __init__(self, lr=0.01):", "        self.lr = lr"]
cols = [(86, 156, 214), (206, 145, 120), (220, 220, 170), (156, 220, 254)]
for i, line in enumerate(code): d.text((250, 60 + i * 26), "%3d  %s" % (i + 1, line), font=mono, fill=random.choice(cols))
imgs["code_editor"] = im

im, d = new((255, 255, 255)); titlebar(d, "Budget 2026.xlsx - Excel")
d.rectangle([0, 36, W, 80], fill=(33, 115, 70))
heads = ["Item", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Total"]
items = ["Rent", "Food", "Travel", "Power", "Water", "Phone", "Gym", "Books"]
for c in range(9):
    for r in range(22):
        x, y = 60 + c * 135, 100 + r * 27
        d.rectangle([x, y, x + 135, y + 27], outline=(210, 210, 210))
        txt = heads[c] if r == 0 else (items[r % 8] if c == 0 else "{:,}".format(random.randint(100, 9999)))
        d.text((x + 6, y + 3), txt, font=ui if r else uib, fill=(20, 20, 20))
imgs["spreadsheet"] = im

im, d = new((0, 0, 0)); titlebar(d, "Ocean Documentary - Video Player", True)
for y in range(60, 620): d.line([(0, y), (W, y)], fill=(10, 60 + y // 6, 120 + y // 8))
d.ellipse([560, 260, 720, 420], outline=(255, 255, 255), width=6); d.polygon([(615, 295), (615, 385), (690, 340)], fill=(255, 255, 255))
d.rectangle([40, 650, W - 40, 658], fill=(80, 80, 80)); d.rectangle([40, 650, 520, 658], fill=(230, 30, 30))
d.text((40, 670), "12:41 / 48:03", font=ui, fill=(230, 230, 230))
imgs["video_player"] = im

im, d = new((245, 245, 248)); titlebar(d, "Chat - Assistant")
msgs = [("user", "Can you summarise this research paper for me?"), ("bot", "Sure. The paper proposes a new method for training small language models on device."),
        ("user", "What are the main results?"), ("bot", "It matches larger models on three benchmarks while using a tenth of the memory.")]
y = 70
for who, t in msgs:
    x = 640 if who == "user" else 60
    fg = (255, 255, 255) if who == "user" else (20, 20, 20)
    d.rounded_rectangle([x, y, x + 560, y + 80], 18, fill=(0, 120, 212) if who == "user" else (255, 255, 255))
    d.text((x + 18, y + 14), t[:52], font=ui, fill=fg); d.text((x + 18, y + 42), t[52:], font=ui, fill=fg); y += 110
d.rounded_rectangle([60, 640, W - 60, 690], 20, outline=(160, 160, 160), width=2); d.text((84, 652), "Message the assistant...", font=ui, fill=(140, 140, 140))
imgs["chat_ai"] = im

im, d = new((255, 255, 255)); titlebar(d, "Inbox - Mail")
d.rectangle([0, 36, 240, H], fill=(243, 242, 241))
for i, f in enumerate(["Inbox", "Sent", "Drafts", "Archive", "Spam"]): d.text((24, 60 + i * 34), f, font=ui, fill=(40, 40, 40))
senders = ["Amazon", "GitHub", "Bank", "Mom", "Airline", "Newsletter", "Boss", "Dentist", "Landlord", "School"]
subj = ["Your order has shipped", "New pull request review", "Monthly statement ready", "Dinner on Sunday?", "Flight check-in open",
        "This week in tech", "Meeting moved to 3pm", "Appointment reminder", "Rent receipt", "Parent evening"]
for i in range(10):
    y = 60 + i * 64; d.line([(240, y + 60), (W, y + 60)], fill=(225, 225, 225))
    d.text((270, y + 6), senders[i], font=uib, fill=(20, 20, 20)); d.text((270, y + 34), subj[i], font=ui, fill=(90, 90, 90))
imgs["email_inbox"] = im

im, d = new((12, 12, 12)); titlebar(d, "Windows PowerShell", True)
lines = ["PS C:\\> ping 8.8.8.8", "Reply from 8.8.8.8: bytes=32 time=14ms TTL=117", "Reply from 8.8.8.8: bytes=32 time=13ms TTL=117",
         "PS C:\\> git status", "On branch main", "Changes not staged for commit:", "  modified:   src/app.cs", "PS C:\\> dotnet build",
         "Build succeeded.", "    0 Warning(s)", "    0 Error(s)", "PS C:\\> _"]
for i, l in enumerate(lines): d.text((20, 56 + i * 26), l, font=mono, fill=(200, 200, 200))
imgs["terminal"] = im

im, d = new((255, 255, 255)); titlebar(d, "Sales Report Q3 - Dashboard")
d.text((60, 60), "Quarterly revenue by region", font=uib, fill=(20, 20, 20))
d.line([(100, 640), (1200, 640)], fill=(0, 0, 0), width=2); d.line([(100, 120), (100, 640)], fill=(0, 0, 0), width=2)
bars = [("North", 420), ("South", 310), ("East", 480), ("West", 260), ("Online", 500)]
colors = [(66, 133, 244), (219, 68, 55), (244, 180, 0), (15, 157, 88), (171, 71, 188)]
for i, (lab, v) in enumerate(bars):
    x = 170 + i * 205; d.rectangle([x, 640 - v, x + 120, 640], fill=colors[i]); d.text((x + 20, 650), lab, font=ui, fill=(20, 20, 20))
imgs["bar_chart"] = im

im, d = new((232, 226, 214)); titlebar(d, "Maps")
for _ in range(40):
    x0, y0 = random.randint(0, W), random.randint(36, H)
    d.line([(x0, y0), (x0 + random.randint(-400, 400), y0 + random.randint(-300, 300))], fill=(255, 255, 255), width=random.choice([4, 8, 12]))
d.ellipse([300, 300, 520, 460], fill=(170, 210, 160)); d.polygon([(900, 500), (1100, 420), (1250, 700), (950, 700)], fill=(160, 200, 230))
d.ellipse([620, 330, 660, 370], fill=(220, 40, 40)); d.text((672, 336), "Central Station", font=uib, fill=(30, 30, 30))
d.rounded_rectangle([20, 50, 420, 96], 10, fill=(255, 255, 255)); d.text((36, 60), "Directions to Central Station", font=ui, fill=(60, 60, 60))
imgs["map"] = im

im, d = new((240, 240, 240)); titlebar(d, "Sign in")
d.rounded_rectangle([440, 140, 840, 600], 16, fill=(255, 255, 255)); d.text((560, 170), "Sign in", font=uib, fill=(20, 20, 20))
for i, lab in enumerate(["Email address", "Password"]):
    y = 260 + i * 110; d.text((480, y), lab, font=ui, fill=(70, 70, 70)); d.rounded_rectangle([480, y + 30, 800, y + 72], 8, outline=(150, 150, 150), width=2)
d.text((494, 410), "**********", font=ui, fill=(30, 30, 30)); d.rounded_rectangle([480, 500, 800, 548], 8, fill=(0, 103, 192)); d.text((610, 510), "Sign in", font=uib, fill=(255, 255, 255))
imgs["login_form"] = im

im, d = new((255, 255, 255)); titlebar(d, "INV-4471.pdf - PDF Reader")
d.text((120, 80), "INVOICE  #4471", font=font("segoeuib.ttf", 44), fill=(20, 20, 20))
d.text((120, 150), "Acme Corporation  -  Billed to: Northwind Ltd", font=ui, fill=(60, 60, 60))
rows = [("Consulting services", "4,200.00"), ("Cloud hosting", "1,150.00"), ("Support plan", "600.00"), ("TOTAL DUE", "5,950.00")]
for i, (a, b) in enumerate(rows):
    y = 240 + i * 50; f2 = uib if i == 3 else ui
    d.line([(120, y + 40), (1160, y + 40)], fill=(220, 220, 220)); d.text((130, y), a, font=f2, fill=(20, 20, 20)); d.text((1000, y), b, font=f2, fill=(20, 20, 20))
d.text((120, 520), "Due date: 30 November 2026", font=ui, fill=(160, 30, 30))
imgs["invoice"] = im

for k, v in imgs.items(): v.save(out / (k + ".png"))
queries = {
    "code_editor": ["python code in a code editor", "đoạn mã Python trong trình soạn thảo"],
    "spreadsheet": ["spreadsheet with a monthly budget", "表格中的月度预算"],
    "video_player": ["watching an ocean documentary video", "video về đại dương"],
    "chat_ai": ["chatting with an AI assistant about a research paper", "mit einem KI-Assistenten chatten"],
    "email_inbox": ["email inbox with messages", "boîte de réception des e-mails"],
    "terminal": ["command line terminal running git and ping", "ターミナルのコマンド"],
    "bar_chart": ["bar chart of quarterly revenue by region", "biểu đồ doanh thu theo vùng"],
    "map": ["map with directions to the station", "mapa con indicaciones"],
    "login_form": ["login page asking for email and password", "trang đăng nhập"],
    "invoice": ["invoice 4471 from Acme", "hóa đơn số 4471"],
}
negatives = ["a cat sleeping on a sofa", "mountain landscape at sunset", "a plate of spaghetti"]
json.dump({"images": sorted(imgs), "queries": queries, "negatives": negatives},
          open(out / "set.json", "w", encoding="utf-8"), ensure_ascii=False, indent=1)
print(len(imgs), "images", sum(len(v) for v in queries.values()), "queries", len(negatives), "negatives")
