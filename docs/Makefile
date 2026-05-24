# Sphinx documentation Makefile for shoestring-wm.
#
# Common usage:
#   make html   — build the HTML user guide  (output: _build/html/)
#   make man    — build all man pages        (output: _build/man/)
#   make clean  — wipe the _build/ tree
#   make help   — list every builder Sphinx knows about

SPHINXOPTS    ?= -W --keep-going
SPHINXBUILD   ?= sphinx-build
SOURCEDIR     = .
BUILDDIR      = _build

.PHONY: help Makefile

# Catch-all: route every target ("make html", "make man", "make clean", ...)
# through sphinx-build's -M dispatch so we don't enumerate builders here.
%: Makefile
	@$(SPHINXBUILD) -M $@ "$(SOURCEDIR)" "$(BUILDDIR)" $(SPHINXOPTS) $(O)
