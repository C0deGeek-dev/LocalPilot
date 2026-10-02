_PAIRS = [
    (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"),
    (100, "C"), (90, "XC"), (50, "L"), (40, "XL"),
    (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
]


def to_roman(n: int) -> str:
    """Return n (1..3999) in standard subtractive Roman notation."""
    if isinstance(n, bool) or not isinstance(n, int):
        raise ValueError(f"not an integer: {n!r}")
    if not 1 <= n <= 3999:
        raise ValueError(f"out of range 1..3999: {n}")
    out = []
    for value, digits in _PAIRS:
        count, n = divmod(n, value)
        out.append(digits * count)
    return "".join(out)
