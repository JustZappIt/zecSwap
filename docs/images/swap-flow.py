"""Draws the README's sequence diagram of a ZEC to USDC swap, light and dark, as HTML pages to screenshot:

    python3 docs/images/swap-flow.py "$dir"
    for t in light dark; do "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new \
        --hide-scrollbars --force-device-scale-factor=2 --window-size=1190,870 \
        --screenshot=docs/images/swap-flow-$t.png "file://$dir/$t.html"; done
"""
import html
import sys

W, H = 1190, 870
LANES = {"you": 101, "zec": 330, "eth": 570, "maker": 799}
HEAD_W, HEAD_H, HEAD_Y = 164, 39, 25
ROW0, STEP = 112, 56

THEMES = {
    "light": dict(bg="#ffffff", frame="#d0d0d0", text="#111111", sub="#555555", life="#c8c8c8",
                  badge_bg="#111111", badge_fg="#ffffff", box_bg="#f4f4f4",
                  you="#0f7a64", zec="#f4b72e", eth="#1f5fc8", maker="#6a55c9", zec_fg="#111111",
                  head_fg="#ffffff", gray="#555555", amber="#b07d10", nr="#b5480b"),
    "dark": dict(bg="#1b1b1b", frame="#333333", text="#f2f2f2", sub="#a8a8a8", life="#4a4a4a",
                 badge_bg="#f2f2f2", badge_fg="#111111", box_bg="#232323",
                 you="#3cc9b0", zec="#f4b72e", eth="#6aa8f0", maker="#a996f2", zec_fg="#111111",
                 head_fg="#111111", gray="#9a9a9a", amber="#f4b72e", nr="#f0884a"),
}

HEADS = [("you", "You (Veylor)"), ("zec", "Zcash"), ("eth", "Ethereum contract"), ("maker", "Maker")]

# (kind, from, to, label, sublabel, colour key, dashed)
STEPS = [
    ("arrow", "maker", "you", "Quote, plus proof it knows e", "e itself stays secret", "maker", False),
    ("arrow", "you", "maker", "Your Z, proof you know z, payout note", "z itself stays on your phone", "you", False),
    ("arrow", "maker", "eth", "open()", "escrows USDC under E and Z", "maker", False),
    ("box", "you", None, "Check every term on chain", None, "you", False),
    ("arrow", "you", "zec", "Send ZEC", "to the joint address e + z", "you", False),
    ("box", "maker", None, "Wait for 2 confirmations", None, "maker", False),
    ("arrow", "maker", "eth", "ready()", "only before t0; no maker refund until t1", "maker", False),
    ("box", "you", None, "Wait until ready() is 2 blocks deep", None, "you", False),
    ("arrow", "you", "eth", "claim(z), through Flashbots Protect", "reveals your half, unseen until mined", "you", False),
    ("box", "eth", None, "Checks z·G = Z on Pallas", None, "eth", False),
    ("arrow", "eth", "you", "0zkUSDC into your Railgun note", "only the note fixed at open", "eth", False),
    ("arrow", "eth", "maker", "Reads z", "any time after the claim", "eth", True),
    ("arrow", "maker", "zec", "Spends the ZEC with e + z", "swap complete", "maker", False),
]

# (first step, last step, colour key, title, lines)
PHASES = [
    (1, 4, "gray", "BEFORE YOU PAY", ["Maker refund: until t0", "You claim: not yet", "Nothing of yours is at risk."]),
    (5, 6, "amber", "ZEC SENT, NOT READY",
     ["Maker refund: until t0,", "and your ZEC comes back to you", "You claim: from t0 if never readied"]),
    (7, 8, "you", "READY, YOUR TURN", ["Maker refund: not until t1", "You claim: now, until t1"]),
    (9, 13, "eth", "CLAIMED, FINAL",
     ["Maker refund: never", "Steps 11 and 12 to 13 happen", "in either order."]),
]
NO_RETURN_AFTER = 8

SANS = "-apple-system, 'Inter', 'Helvetica Neue', Arial, sans-serif"
MONO = "'SF Mono', Menlo, 'JetBrains Mono', monospace"


def row_y(i):
    return ROW0 + STEP * (i - 1)


def text(x, y, s, size, fill, weight=400, anchor="middle", family=SANS, spacing=0):
    return (f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}" '
            f'fill="{fill}" text-anchor="{anchor}" letter-spacing="{spacing}">{html.escape(s)}</text>')


def badge(cx, cy, n, t):
    w = 18 if n < 10 else 22
    return (f'<rect x="{cx - w / 2}" y="{cy - 9}" width="{w}" height="18" fill="{t["badge_bg"]}"/>'
            + text(cx, cy + 4.5, str(n), 11, t["badge_fg"], 700, family=MONO))


def draw(t):
    out = [f'<rect width="{W}" height="{H}" fill="{t["bg"]}"/>',
           f'<rect x="0.5" y="0.5" width="{W - 1}" height="{H - 1}" fill="none" stroke="{t["frame"]}"/>']
    for key, name in HEADS:
        x = LANES[key]
        out.append(f'<line x1="{x}" y1="{HEAD_Y + HEAD_H}" x2="{x}" y2="{H - 48}" stroke="{t["life"]}" '
                   f'stroke-width="1.3" stroke-dasharray="4 4"/>')
        out.append(f'<rect x="{x - HEAD_W / 2}" y="{HEAD_Y}" width="{HEAD_W}" height="{HEAD_H}" fill="{t[key]}"/>')
        fg = t["zec_fg"] if key == "zec" else t["head_fg"]
        out.append(text(x, HEAD_Y + 25, name, 13, fg, 700))

    nr_y = (row_y(NO_RETURN_AFTER) + row_y(NO_RETURN_AFTER + 1)) / 2
    out.append(f'<line x1="11" y1="{nr_y}" x2="{W - 11}" y2="{nr_y}" stroke="{t["nr"]}" stroke-width="1.3" '
               f'stroke-dasharray="4 4"/>')
    out.append(text(W - 12, nr_y - 6, "POINT OF NO RETURN", 11, t["nr"], 700, "end", MONO, 1))

    for i, (kind, src, dst, label, sub, colour, dashed) in enumerate(STEPS, start=1):
        y, c, x0 = row_y(i), t[colour], LANES[src]
        if kind == "box":
            w = 40 + len(label) * 6.5
            left = max(4, min(x0 - w / 2, W - 12 - w))
            out.append(f'<rect x="{left}" y="{y - 15.5}" width="{w}" height="31" fill="{t["box_bg"]}" '
                       f'stroke="{c}" stroke-width="1.5"/>')
            out.append(badge(left + 15, y, i, t))
            out.append(text(left + 30 + (w - 34) / 2, y + 4.5, label, 12.5, t["text"], 500))
            continue
        x1 = LANES[dst]
        tip = x1 - 8 if x1 > x0 else x1 + 8
        head = 9 if x1 > x0 else -9
        dash = ' stroke-dasharray="6 4"' if dashed else ""
        out.append(f'<line x1="{x0}" y1="{y}" x2="{tip - head}" y2="{y}" stroke="{c}" stroke-width="2.2"{dash}/>')
        out.append(f'<path d="M{tip} {y} L{tip - head} {y - 5} L{tip - head} {y + 5} Z" fill="{c}"/>')
        mid = (x0 + x1) / 2
        out.append(text(mid, y - 9, label, 13, t["text"], 600))
        out.append(text(mid, y + 18, sub, 11.5, t["sub"]))
        out.append(badge(x0, y, i, t))

    bx = 907
    for first, last, colour, title, lines in PHASES:
        c = t[colour]
        top, bottom = row_y(first) - 20, row_y(last) + 20
        out.append(f'<path d="M{bx - 7} {top} H{bx} V{bottom} H{bx - 7}" fill="none" stroke="{c}" stroke-width="2"/>')
        block = 15 * (len(lines) + 1)
        y = (top + bottom) / 2 - block / 2 + 11
        out.append(text(bx + 20, y, title, 11, c, 700, "start", MONO, 1.2))
        for line in lines:
            y += 15
            out.append(text(bx + 20, y, line, 12, t["text"], 400, "start"))
    return (f'<!doctype html><meta charset="utf-8"><style>html,body{{margin:0;background:{t["bg"]}}}</style>'
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">'
            + "".join(out) + "</svg>")


for name, theme in THEMES.items():
    with open(f"{sys.argv[1]}/{name}.html", "w") as f:
        f.write(draw(theme))
