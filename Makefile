TARGET := riscv64imac-unknown-none-elf
CARGO_TARGET_DIR ?= $(CURDIR)/target/teaching
export CARGO_TARGET_DIR
CARGO := cargo
ELF := $(CARGO_TARGET_DIR)/$(TARGET)/release/suba
OBJCOPY ?= llvm-objcopy
STAGE ?= 6
TRACE ?= 1
ifeq ($(TRACE),0)
TRACE_FLAGS := --no-default-features
TRACE_CHECK := --no-trace
endif
QEMU ?= qemu-system-riscv64
QEMU_ARGS := -machine virt -m 128M -smp 2 -bios default -display none -serial stdio -monitor none -net none -no-reboot
.DEFAULT_GOAL := build
.PHONY: all build run check-teaching test host-test budget fmt clean
all: build

build:
	cd os && $(CARGO) build --locked --release $(TRACE_FLAGS) --target $(TARGET)
	$(OBJCOPY) -O binary "$(ELF)" "$(ELF).bin"

run: build
	$(QEMU) $(QEMU_ARGS) -kernel "$(ELF).bin"

host-test:
	cd os && $(CARGO) test --locked --workspace --exclude suba --target "$$(rustc -vV | sed -n 's/^host: //p')"

check-teaching: host-test budget
	cd os && $(CARGO) build --locked --release $(TRACE_FLAGS) --target $(TARGET) --features selftest
	python3 scripts/check_isa.py "$(ELF)"
	$(OBJCOPY) -O binary "$(ELF)" "$(ELF).check.bin"
	python3 scripts/check_teaching.py --stage $(STAGE) -- $(QEMU) $(QEMU_ARGS) -kernel "$(ELF).check.bin"
	$(MAKE) build
	python3 scripts/check_isa.py "$(ELF)"
	python3 scripts/check_teaching.py --shell $(TRACE_CHECK) --stage $(STAGE) -- $(QEMU) $(QEMU_ARGS) -kernel "$(ELF).bin"

test: check-teaching

budget:
	python3 scripts/budget.py

fmt:
	cd os && $(CARGO) fmt --all

clean:
	cd os && $(CARGO) clean
