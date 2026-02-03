AGENTS_DIR := $(HOME)/.agents/skills
CLAUDE_DIR := $(HOME)/.claude/skills
SKILL_DIRS := $(shell find . -maxdepth 2 -name 'SKILL.md' -exec dirname {} \; | sed 's|^\./||' | sort)

.PHONY: install uninstall list

install:
	@mkdir -p $(AGENTS_DIR) $(CLAUDE_DIR)
	@for skill in $(SKILL_DIRS); do \
		for dir in $(AGENTS_DIR) $(CLAUDE_DIR); do \
			if [ -L "$$dir/$$skill" ]; then \
				echo "skip: $$skill in $$dir (already linked)"; \
			elif [ -e "$$dir/$$skill" ]; then \
				echo "skip: $$skill in $$dir (exists but not a symlink)"; \
			else \
				ln -s "$(CURDIR)/$$skill" "$$dir/$$skill"; \
				echo "link: $$skill -> $$dir"; \
			fi \
		done \
	done

uninstall:
	@for skill in $(SKILL_DIRS); do \
		for dir in $(AGENTS_DIR) $(CLAUDE_DIR); do \
			if [ -L "$$dir/$$skill" ]; then \
				rm "$$dir/$$skill"; \
				echo "remove: $$skill from $$dir"; \
			fi \
		done \
	done

list:
	@echo "Skills in this repo:"
	@for skill in $(SKILL_DIRS); do echo "  /$$skill"; done
	@echo ""
	@echo "Installed skills ($(AGENTS_DIR)):"
	@ls -1 $(AGENTS_DIR) 2>/dev/null | while read s; do \
		if [ -L "$(AGENTS_DIR)/$$s" ]; then \
			target=$$(readlink "$(AGENTS_DIR)/$$s"); \
			echo "  /$$s -> $$target"; \
		else \
			echo "  /$$s (local)"; \
		fi \
	done
