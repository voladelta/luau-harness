from . import blocks, html


def convert(text):
    return html.render(blocks.parse(text))
