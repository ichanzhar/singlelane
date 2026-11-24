# CLAUDE.md

## What this repo is
<One paragraph: service purpose, main modules, where the entry points are.>

## Commands (the agent MUST run these before finishing)
- Build:  `./gradlew build -x test`
- Test:   `./gradlew test`
- Lint:   `./gradlew ktlintCheck detekt`
- Web:    `cd web && npm ci && npm run lint && npm test`

## Conventions
- Kotlin: <style rules, error handling pattern, DTO/entity rules, Jimmer usage notes>
- Tests: <JUnit5 + Kotest? Testcontainers allowed? naming>
- Commits: conventional commits (`feat:`, `fix:`, `chore:` ...)

## Definition of done
- Every acceptance criterion in the issue has a test.
- Build, lint and tests pass locally.
- No new dependencies without justification in the PR description.

## Do not touch
- `.github/workflows/`, `infra/`, `*.tf`, secrets, migrations already released (add new ones instead).
