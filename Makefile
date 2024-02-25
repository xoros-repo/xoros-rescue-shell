prog :=xoros-rescue-shell

debug ?=

$(info debug is $(debug))

ifdef debug
  release :=
  target :=debug
  extension :=debug
else
  release :=--release
  target :=release
  extension :=
endif

build:
	cargo build $(release)

install:
	install -Dm 0644 target/$(target)/$(prog) /usr/bin/$(prog)$(extension)

all: build install

help:
	@echo "usage: make $(prog) [debug=1]"