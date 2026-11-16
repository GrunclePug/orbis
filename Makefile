include config.mk

.PHONY: all build run test fmt lint clean sweep install uninstall gui-shot

all: build

build: sweep
	$(CARGO) build --release --locked

run: sweep
	$(CARGO) run -- $(ARGS)

test: sweep
	$(CARGO) test --workspace --all-targets
	$(CARGO) test --workspace --doc

fmt:
	$(CARGO) fmt --all

lint: sweep
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

# Render the GUI without a window and write a screenshot and a state dump.
gui-shot: sweep
	$(CARGO) run -- gui --headless --frames 4 --screenshot target/gui.png --dump target/gui.txt $(ARGS)

clean:
	$(CARGO) clean

sweep:
	@size=$$(du -s --block-size=1G target 2>/dev/null | cut -f1); \
	if [ -n "$$size" ] && [ "$$size" -ge $(TARGET_MAX) ]; then \
		echo "target/ is $${size}G, sweeping workspace builds"; \
		$(CARGO) clean --workspace; \
		size=$$(du -s --block-size=1G target 2>/dev/null | cut -f1); \
		if [ -n "$$size" ] && [ "$$size" -ge $(TARGET_MAX) ]; then $(CARGO) clean; fi; \
	fi

install:
	install -Dm755 target/release/$(APP) $(DESTDIR)$(PREFIX)/bin/$(APP)

uninstall:
	$(RM) $(DESTDIR)$(PREFIX)/bin/$(APP)
