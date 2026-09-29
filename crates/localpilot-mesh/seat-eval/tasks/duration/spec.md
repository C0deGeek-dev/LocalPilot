# Task: parse durations

Create `duration.py` with `parse_duration(s: str) -> int` returning seconds for strings made of one or more `<integer><unit>` parts, units `h`, `m`, `s`, in that order, each at most once, no spaces (e.g. `1h30m`, `45s`, `2h5s`). Raise `ValueError` for anything else (empty, unknown unit, wrong order, repeated unit, spaces, negative). Add tests in `test_duration.py`.
