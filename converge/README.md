# converge

Compiled TypeScript implementation backing the `/converge` skill. Iterates observe→decide→act loops against a pluggable fitness skill until the target is reached, actions are exhausted, or the iteration cap is hit.

See [`SKILL.md`](./SKILL.md) for the skill contract.

## Development

```bash
npm install
npm run dev              # run via tsx (no build)
npm run typecheck        # tsc --noEmit
npm run lint             # eslint + prettier
npm run format           # prettier --write
npm run test             # unit tests
npm run build            # produce dist/
npm run check            # typecheck + lint + test
```
