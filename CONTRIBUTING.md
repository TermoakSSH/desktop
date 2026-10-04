# Contributing to Termoak

Thanks for helping! Bug reports, fixes, features and translations are all
welcome.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree on
  the approach.
- Code, comments, commit messages and documentation are written in English.
- User-visible text never goes straight into the code: it goes into the
  app's translation files (see [docs/I18N.md](https://github.com/TermoakSSH/core/blob/main/docs/I18N.md)), at least in
  English.

## Adding or improving a language

Translations need no programming. Copy the English strings file of the app
(see the table in [docs/I18N.md](https://github.com/TermoakSSH/core/blob/main/docs/I18N.md)) to your language code,
translate the values and open a pull request. Keep the keys and the
placeholders (`%{name}`, `%1$s`, `%@`) exactly as they are.

## Pull requests

- Keep each pull request focused on one change.
- Run the formatter and the tests of the part you touched (`cargo fmt`,
  `cargo test`, Gradle or Xcode builds) before opening it.
- Describe what changes for the user and how you tested it.

## Contributor License Agreement

Termoak is developed by Ohz Digital SL and released under the
[GNU Affero General Public License v3.0](LICENSE). By submitting a
contribution (code, documentation, translations or any other material) you
agree to the following:

1. You wrote the contribution yourself, or you otherwise have the right to
   submit it under these terms.
2. You license your contribution to the public under the AGPL-3.0, the same
   license as the project.
3. You also grant Ohz Digital SL a perpetual, worldwide, non-exclusive,
   royalty-free, irrevocable license to use, modify, distribute and sublicense
   your contribution under any license terms, including commercial ones. This
   lets Ohz Digital SL publish the apps in app stores and offer separate
   licenses to companies that need them, while the project itself always
   stays available under the AGPL-3.0.
4. You keep the copyright of your contribution.

If you cannot agree to these terms, please tell us in the issue or pull
request before submitting the contribution.

## Trademarks

The Termoak name and logo are not covered by the code license. See
[TRADEMARK.md](TRADEMARK.md).
