# Architectural Decision Records

Settled policy questions about shoestring-wm: the ones likely to come up
again as "should we change this?" in a later session that lacks the
context of the one that settled them. Read the relevant ADR before
proposing to change what it covers.

Not in the Sphinx toctree; read these files directly.

## When to write one

When a question is settled that isn't obvious from the code, isn't
already written in `docs/` or `AGENTS.md`, and is likely to be asked
again. Commit messages describe one piece of work; an ADR records a
standing policy that should outlive it.

## Format

`NNNN-short-slug.md`, numbered in sequence, never renumbered or deleted.
Each file has:

- **Status**: Accepted, with the date. A later ADR may supersede an
  earlier one; the earlier one stays as the record, with a note pointing
  to its replacement, instead of being edited in place.
- **Context**: what prompted the question, in enough detail that the
  decision doesn't look arbitrary.
- **Decision**: the rule, stated so it can be applied without re-deriving
  the reasoning.
- **Consequences**: what the decision does and doesn't justify.

## Index

- [0001: The public tree is a published artifact; it ships sanitized and
  records the provenance of what it borrows](0001-the-public-tree-is-a-published-artifact.md)
- [0002: Depend on upstream Smithay at a pinned commit; patch a single
  crate under `third_party/` instead of forking](0002-patch-a-crate-instead-of-forking-smithay.md)
