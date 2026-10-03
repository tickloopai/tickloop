DEV_LAUNCHER := $(shell dirname '$(subst ','"'"',$(strip $(MAKEFILE_LIST)))')/scripts/dev-agent.sh
# 本地开发入口。密钥放在仓库根目录的 .env（已被 .gitignore 忽略），例如：
#   DEEPSEEK_API_KEY=sk-...
-include .env
export

# dev starts the Agent in the current directory and keeps its state isolated.
DEV_CONFIG_DIR ?= /tmp/tickloop-dev/config
ARGS ?=

CARGO_PACKAGE := -p tickloop-agent
PACKAGE_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
PACKAGE_ARCHIVE = dist/tickloop-$(PACKAGE_TARGET).tar.gz

.PHONY: dev dev-reset build install package test check release-docs prototype-glyph-set prototype-glyph-set-terminal prototype-earnings-review

## Start in the current directory with isolated state; example: make dev ARGS="-c"
dev:
	@mkdir -p "$(DEV_CONFIG_DIR)"
	TICKLOOP_CONFIG_DIR="$(DEV_CONFIG_DIR)" "$(DEV_LAUNCHER)" $(ARGS)

## Remove only the dev state directory
dev-reset:
	rm -rf "$(DEV_CONFIG_DIR)"

build:
	cargo build --locked $(CARGO_PACKAGE) -p tickloop-ticks --bins

install:
	cargo install --locked --path crates/agent --bins
	cargo install --locked --path crates/ticks --bin tick

## Package the Agent and example Ticks.
package:
	cargo build --locked --release $(CARGO_PACKAGE) -p tickloop-ticks --bins --target "$(PACKAGE_TARGET)" --target-dir target
	@mkdir -p dist
	tar -czf "$(PACKAGE_ARCHIVE)" -C "target/$(PACKAGE_TARGET)/release" tickloop tick
	@printf 'Package: %s\n' "$(PACKAGE_ARCHIVE)"

test:
	@mkdir -p /tmp/tickloop-test
	TICKLOOP_CONFIG_DIR=$$(mktemp -d /tmp/tickloop-test/config.XXXXXX) \
	TICKLOOP_TMPDIR=$$(mktemp -d /tmp/tickloop-test/tmp.XXXXXX) \
	NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
	env -u HERDR_ENV -u HERDR_SOCKET_PATH -u HERDR_PANE_ID \
	cargo test --locked --workspace

check:
	cargo fmt --all --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	$(MAKE) test

## 发布已提交的 Docs 快照；默认递增 patch，可用 VERSION=0.1.0 指定版本
release-docs:
	@node services/artifacts/scripts/release.mjs

## 启动一次性 Glyph Set 设置页原型；访问 http://127.0.0.1:8765/?variant=A
prototype-glyph-set:
	python3 -m http.server 8765 --directory .scratch/agent/prototypes/glyph-set-preview

## 在真实终端中并排检查 Unicode 与 Nerd Font 的占格、基线和 spinner
prototype-glyph-set-terminal:
	python3 .scratch/agent/prototypes/glyph-set-preview/terminal_probe.py

## 启动财报复盘原型；访问 http://127.0.0.1:8766/earnings-review.html?variant=A
prototype-earnings-review:
	python3 -m http.server 8766 --directory .scratch/earnings-review/prototype
