# Third-party notices

reman is licensed under the Apache License 2.0 (see LICENSE). It includes or adapts the following
work by others, whose licenses require this notice. The Rust libraries it is built from are listed,
with their licenses, in THIRD_PARTY_LICENSES.html, which ships with every release.

## tldr-pages

https://github.com/tldr-pages/tldr

The command descriptions reman shows and searches (`tldr_map.json`, built into the program) are
taken from the tldr-pages: each command's one-line summary and example descriptions, extracted
and shortened. Copyright © 2014-present the tldr-pages team and contributors, licensed under the
Creative Commons Attribution 4.0 International License (CC BY 4.0):
https://creativecommons.org/licenses/by/4.0/

## ONNX Runtime

https://github.com/microsoft/onnxruntime

Built into the program to run the search model. Copyright (c) Microsoft Corporation. MIT License
(the same terms as the MIT License text below, with that copyright line).

## SQLite

https://sqlite.org

Built into the program as its database. SQLite is in the public domain.

## bge-small-en-v1.5

https://huggingface.co/BAAI/bge-small-en-v1.5

The search model, downloaded on first start (not part of the program). Copyright (c) BAAI. MIT
License (the same terms as the MIT License text below, with that copyright line).

## Atuin

https://github.com/atuinsh/atuin

The provider token patterns in `rust/src/redact.rs` (AWS, GitHub, GitLab, Slack, Stripe, Netlify,
npm, Pulumi) are adapted from Atuin's `crates/atuin-common/src/secrets.rs`. The design of
`rust/src/pty.rs` (`reman shell`: the shell in a pseudo-terminal, private OSC marks around each
command's output, the screen read through a VT emulator between them) follows Atuin's
`crates/atuin-pty-proxy`.

```
MIT License

Copyright (c) 2021 Ellie Huxtable

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
