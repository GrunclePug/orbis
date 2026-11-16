# orbis build configuration, included by the Makefile.

APP = orbis
PREFIX ?= /usr/local
CARGO ?= cargo
# Keep target/ under this many GiB (see the sweep target).
TARGET_MAX ?= 6
