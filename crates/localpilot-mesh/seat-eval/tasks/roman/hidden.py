from roman import to_roman
cases={1:"I",4:"IV",9:"IX",14:"XIV",40:"XL",90:"XC",400:"CD",900:"CM",1994:"MCMXCIV",2024:"MMXXIV",3999:"MMMCMXCIX"}
for n,r in cases.items(): assert to_roman(n)==r,(n,to_roman(n))
for bad in (0,-1,4000,2.5,"3",True,None):
    try: to_roman(bad)
    except ValueError: pass
    else: raise AssertionError(f"no ValueError for {bad!r}")
print("HIDDEN_OK")
