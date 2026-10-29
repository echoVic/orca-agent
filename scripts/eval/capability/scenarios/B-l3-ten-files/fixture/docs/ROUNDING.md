# Rounding policy

All monetary rounding in this pipeline uses **round half to even** (banker's rounding): a value
exactly halfway between two cents goes to the even cent, so `0.5 -> 0`, `1.5 -> 2`, `2.5 -> 2`.

`round_half_up` is a legacy helper and must not be used by new code.
