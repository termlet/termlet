# Use of the Termlet name and logo

[简体中文](TRADEMARK.md) · **English**

Termlet's source code is licensed under the [GNU AGPL-3.0-or-later](LICENSE), but that license does not grant trademark rights (section 7(e) of the AGPL allows declining to grant rights under trademark law). The name "Termlet" and its logo — the icon in [`docs/brand/`](docs/brand/) (a `>` prompt with a leaf), the wordmark and the combined mark, as well as the program icons and installer images generated from them — **are not licensed under the AGPL**. This page explains when you may use them.

This page is not part of the license and does not restrict any right the AGPL gives you: you may still copy, modify and distribute Termlet's code under the AGPL; when you distribute it, use your own name and logo.

## What you may do without asking

- **Refer to Termlet truthfully**: mention Termlet in articles, tutorials, reviews, talks and comparison tables, with its logo or screenshots, to refer to Termlet itself.
- **State origin and compatibility**: for example "based on Termlet", "a fork of Termlet", "compatible with Termlet's backup format", "a color scheme for Termlet".
- **Redistribute official releases unchanged**: when you redistribute the installers or archives released by Termlet without modification (for example in an internal software repository or a mirror), keep the name and icons in them.
- **Contribute to Termlet itself**: use the name and logo in changes, issues and discussions for this repository.

## When you need a different name

When you publicly distribute a **modified version** (including forks, rebuilds with code changes, and repackaging with changed defaults):

- use a different name and icon; do not use "Termlet" or a name easily confused with it (such as "Termlet Pro" or "TermletX"), and do not use an icon similar to Termlet's logo;
- do not suggest that your version is official, or released or endorsed by Termlet's maintainer;
- you may truthfully say it is "based on Termlet" and link to this repository.

In addition, to keep your users apart from the official services, we **recommend** (this is not a license requirement) that a modified version also changes:

- the addresses and signing public keys for automatic updates and remote configuration (the `update` and `remote_config` modules of `termlet-core`); otherwise users of your version may have it replaced by official updates;
- the addresses for usage statistics and crash reports (the `usage` and `crash` modules of `termlet-core`); otherwise your users' statistics are sent to Termlet's servers;
- the data folder, the keychain service name, the application ID (`dev.termlet.Termlet`) and the `termlet` command name; otherwise it interferes with a Termlet installed on the same computer.

## Other

- Third-party operating system and brand logos shown in the program and the README (from Simple Icons), and the names of other products, are trademarks of their respective owners; use them according to their owners' rules.
- If you are not sure whether a use is allowed, or want to use the name and logo in a way not listed above, email hi@termlet.dev.
- This page may change; the latest version in this repository applies.
