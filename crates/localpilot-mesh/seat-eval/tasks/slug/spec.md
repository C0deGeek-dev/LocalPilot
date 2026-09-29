# Task: slugify

Create `slug.py` with a function `slugify(text: str, max_len: int = 48) -> str`:
- lowercase the text;
- replace every run of characters that are not ASCII letters or digits with a single `-`;
- strip leading and trailing `-`;
- if longer than `max_len`, cut at `max_len` and strip any trailing `-` again;
- return `"item"` when the result is empty.
Add tests for it in `test_slug.py` (pytest or unittest).
