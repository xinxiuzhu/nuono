"""Build the embedded status font from Google Fonts' NotoSansSC[wght].ttf.

Requires fonttools. Usage: python subset_status_font.py SOURCE.ttf OUTPUT.ttf
"""

import sys
from fontTools import subset
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

font = instantiateVariableFont(TTFont(sys.argv[1]), {"wght": 450}, inplace=True)
options = subset.Options()
options.name_IDs = [0, 1, 2, 3, 4, 5, 6, 13, 14]
subsetter = subset.Subsetter(options=options)
chinese = set()
for lead in range(0xA1, 0xF8):
    for trail in range(0xA1, 0xFF):
        try:
            chinese.update(map(ord, bytes((lead, trail)).decode("gb2312")))
        except UnicodeDecodeError:
            pass
subsetter.populate(
    unicodes=sorted(chinese) + list(range(0x20, 0x7F))
    + list(range(0xA0, 0x100))
    + list(range(0x2000, 0x2070))
    + list(range(0x3000, 0x3100))
    + list(range(0xFF00, 0xFFF0))
)
subsetter.subset(font)
for record in font["name"].names:
    if record.nameID in (1, 3, 4, 6):
        name = "NuonoStatusSans" if record.nameID == 6 else "Nuono Status Sans"
        record.string = name.encode(record.getEncoding())
font.save(sys.argv[2])
