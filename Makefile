PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin

.PHONY: build install uninstall clean

build:
	cargo build --release

install: build
	install -d $(BINDIR)
	install -m 0755 target/release/bfiles $(BINDIR)/bfiles

uninstall:
	rm -f $(BINDIR)/bfiles

clean:
	cargo clean
