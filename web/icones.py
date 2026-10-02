#!/usr/bin/env python3
"""Gera web/icones.woff2: só os ícones usados nas páginas, tirados da Symbols
Nerd Font (licenças em github.com/ryanoasis/nerd-fonts). Ícone novo: acrescente
aqui (nome em glyphnames.json do Nerd Fonts), rode e use a classe `i-<nome>`
das páginas.

    python3 web/icones.py ~/Downloads/SymbolsNerdFont-Regular.ttf

A fonte vem de NerdFontsSymbolsOnly.tar.xz nas releases do Nerd Fonts.
Precisa de fonttools e brotli.
"""
import sys
from pathlib import Path

from fontTools import subset

ICONES = {
    'fa-play': 0xF04B,
    'fa-xmark': 0xF00D,
    'fa-chevron_left': 0xF053,
    'fa-chevron_right': 0xF054,
    'fa-gear': 0xF013,
    'fa-check': 0xF00C,
    'fa-thumbs_up': 0xF164,
    'fa-thumbs_down': 0xF165,
    'fa-heart': 0xF004,
    'fa-trash': 0xF1F8,
    # fatia do círculo = quanto já foi assistido (1/8 a 8/8)
    **{f'md-circle_slice_{n}': 0xF0A9E + n - 1 for n in range(1, 9)},
}

saida = Path(__file__).with_name('icones.woff2')
opts = subset.Options()
opts.flavor = 'woff2'
opts.layout_features = []
opts.name_IDs = ['*']
opts.notdef_outline = True
fonte = subset.load_font(sys.argv[1], opts)
s = subset.Subsetter(opts)
s.populate(unicodes=ICONES.values())
s.subset(fonte)
subset.save_font(fonte, str(saida), opts)
print(f'{saida}: {len(ICONES)} ícones, {saida.stat().st_size} bytes')
