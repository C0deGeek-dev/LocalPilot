from slug import slugify
assert slugify("Hello, World!") == "hello-world"
assert slugify("  --Already--Slugged--  ") == "already-slugged"
assert slugify("Ünïcode ok") == "n-code-ok"
assert slugify("!!!") == "item"
assert slugify("") == "item"
assert slugify("a" * 60) == "a" * 48
assert slugify("abc def", max_len=4) == "abc"
assert slugify("x" * 10, max_len=5) == "xxxxx"
print("HIDDEN_OK")
