SKILLS_DIR := $(HOME)/.agents/skills
SKILL_DIRS := $(shell find . -maxdepth 2 -name 'SKILL.md' -exec dirname {} \; | sed 's|^\./||' | sort)

.PHONY: install uninstall list

install:
	@mkdir -p $(SKILLS_DIR)
	@for skill in $(SKILL_DIRS); do \
		if [ -L "$(SKILLS_DIR)/$$skill" ]; then \
			echo "skip: $$skill (already linked)"; \
		elif [ -e "$(SKILLS_DIR)/$$skill" ]; then \
			echo "skip: $$skill (exists but not a symlink)"; \
		else \
			ln -s "$(CURDIR)/$$skill" "$(SKILLS_DIR)/$$skill"; \
			echo "link: $$skill"; \
		fi \
	done

uninstall:
	@for skill in $(SKILL_DIRS); do \
		if [ -L "$(SKILLS_DIR)/$$skill" ]; then \
			rm "$(SKILLS_DIR)/$$skill"; \
			echo "remove: $$skill"; \
		fi \
	done

list:
	@echo "Skills in this repo:"
	@for skill in $(SKILL_DIRS); do echo "  /$$skill"; done
	@echo ""
	@echo "Installed skills:"
	@ls -1 $(SKILLS_DIR) 2>/dev/null | while read s; do \
		if [ -L "$(SKILLS_DIR)/$$s" ]; then \
			target=$$(readlink "$(SKILLS_DIR)/$$s"); \
			echo "  /$$s -> $$target"; \
		else \
			echo "  /$$s (local)"; \
		fi \
	done
