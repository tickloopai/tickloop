# 本地开发入口。密钥放在仓库根目录的 .env（已被 .gitignore 忽略），例如：
#   DEEPSEEK_API_KEY=sk-...
-include .env
export

# dev 使用独立的状态目录与工作目录，不影响真实的 ~/.tickloop 和当前仓库。
DEV_CONFIG_DIR ?= /tmp/tickloop-dev/config
DEV_PROJECT_DIR ?= /tmp/tickloop-dev/project
ARGS ?=

CARGO_PACKAGE := -p tickloop-agent
PACKAGE_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
PACKAGE_ARCHIVE = dist/tickloop-$(PACKAGE_TARGET).tar.gz

.PHONY: dev dev-here dev-reset build install package test check docs-release docs-release-check prototype-glyph-set prototype-glyph-set-terminal prototype-earnings-review

## 用隔离目录启动 Agent；传参示例：make dev ARGS="-c"
dev:
	@mkdir -p $(DEV_CONFIG_DIR) $(DEV_PROJECT_DIR)
	cargo build -p tickloop-ticks --bin tick
	TICKLOOP_CONFIG_DIR=$(DEV_CONFIG_DIR) cargo run $(CARGO_PACKAGE) -- --cwd $(DEV_PROJECT_DIR) $(ARGS)

## 以当前仓库为工作目录启动，状态仍写入隔离目录
dev-here:
	@mkdir -p $(DEV_CONFIG_DIR)
	cargo build -p tickloop-ticks --bin tick
	TICKLOOP_CONFIG_DIR=$(DEV_CONFIG_DIR) cargo run $(CARGO_PACKAGE) -- $(ARGS)

## 清空 dev 使用的状态目录与工作目录
dev-reset:
	rm -rf $(DEV_CONFIG_DIR) $(DEV_PROJECT_DIR)

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
docs-release:
	@node services/artifacts/scripts/release.mjs

## 仅检查版本、工作树、origin 和 tag，不写入 Git
docs-release-check:
	@node services/artifacts/scripts/release.mjs --check

## 启动一次性 Glyph Set 设置页原型；访问 http://127.0.0.1:8765/?variant=A
prototype-glyph-set:
	python3 -m http.server 8765 --directory .scratch/agent/prototypes/glyph-set-preview

## 在真实终端中并排检查 Unicode 与 Nerd Font 的占格、基线和 spinner
prototype-glyph-set-terminal:
	python3 .scratch/agent/prototypes/glyph-set-preview/terminal_probe.py

## 启动财报复盘原型；访问 http://127.0.0.1:8766/earnings-review.html?variant=A
prototype-earnings-review:
	python3 -m http.server 8766 --directory .scratch/earnings-review/prototype
