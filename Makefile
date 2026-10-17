# Docker-based workflows (see README for running without Docker).
COMPOSE := docker compose

.PHONY: image test test-52 test-61 suite suite-52 suite-61 fuzz bench check

image:
	$(COMPOSE) build dev-61

# pytest parity suite on both supported Django versions
test: test-52 test-61

test-52:
	$(COMPOSE) run --rm -T test-52

test-61:
	$(COMPOSE) run --rm -T test-61

# Django's own template test suite, run through the Rust engine
suite: suite-52 suite-61

suite-52:
	$(COMPOSE) run --rm -T suite-52

suite-61:
	$(COMPOSE) run --rm -T suite-61

# Long differential fuzz run (DTL_FUZZ_ITERS=20000 make fuzz)
fuzz:
	$(COMPOSE) run --rm -T fuzz

# Release-build benchmark vs stock Django
bench:
	$(COMPOSE) run --rm -T bench

# Everything CI runs
check: test suite
	cargo test --release
