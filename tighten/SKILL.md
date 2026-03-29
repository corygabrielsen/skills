---
name: tighten
description: Strip an artifact to its load-bearing words. Specification voice, not explanation voice.
---

# Tighten

The artifact you just produced is too verbose. Compress it.

## The problem

LLMs default to explanatory prose. Many artifacts need specification voice instead.

| Explanation voice                                                                                                            | Specification voice                                                                             |
| ---------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| "The version field should be placed first because deserializers need to match on it before reading the rest of the payload." | "**First byte.** The version is the first field in the type and the first byte(s) on the wire." |
| "This is important because without it, future changes would require a breaking migration."                                   | "Do not merge boundary-crossing types without a version field."                                 |

## Rules

1. **Every word is load-bearing.** Remove a word. Does the meaning change? No → delete it.
2. **State facts, don't explain them.** "X is Y" not "X should be Y because Z."
3. **Name, then rule.** Bold keyword is the handle. The rest of the sentence is the constraint.
4. **No motivation.** The reader doesn't need to know _why_. The rule stands alone.
5. **No examples unless asked.** Examples are explanation in disguise.
6. **No hedging.** "Do not merge X" not "It would be better to avoid merging X."
7. **Context-free.** No jargon that couples the text to a specific domain, language, or framework. If the sentence wouldn't make sense in a different project, it's too coupled.
8. **Imperatives are commands.** Guards, warnings, and constraints are imperative sentences. One sentence. No softening.

## Process

1. Read the artifact.
2. For each sentence: "Is this a fact or an explanation?" Delete explanations.
3. For each remaining word: "Does removing this change the meaning?" Delete if no.
4. For each domain term: "Would this make sense in a different project?" Generalize if no.
5. Check the result reads as a specification, not a tutorial.

## Anti-patterns

| Before                                                               | After                              | Why                                   |
| -------------------------------------------------------------------- | ---------------------------------- | ------------------------------------- |
| "This type is used to represent version numbers across the protocol" | (delete — the type name says this) | Restating what's obvious from context |
| "in order to ensure that"                                            | (delete or replace with nothing)   | Filler                                |
| "it's important to note that"                                        | (delete)                           | Hedging preamble                      |
| "for example, if you wanted to"                                      | (delete unless asked)              | Explanation in disguise               |
| "consensus-affecting logic"                                          | "logic"                            | Domain coupling                       |
| "the struct"                                                         | "the type"                         | Language coupling                     |
| "This allows future versions to..."                                  | (delete)                           | Motivation                            |
