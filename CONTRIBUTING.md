# Contributing to Irmin


## Software Requirements

**Irmin only targets Linux systems**, so development should take place on a Linux computer or virtual machine (i.e. WSL). 

Besides that, Irmin only requires rust >= 1.92.0 or rustup. Rustup is preferred, as it will automatically download the correct toolchain when you first build the project

## Contribution Guidelines

On top of following the [Code of Conduct](https://github.com/elysiae-project/irmin/CODE_OF_CONDUCT.md), there are a few other general rules we'd like to have developers follow to ensure the Elysiae launcher is the best it can be:

### AI Usage

You are allowed to use AI tools to contribute to this project, so long as you review, test and understand the code (in other words, not "Vibe-coded"). Additionally, **ALL** contributors should be writing pull requests by hand to show that they understand what they are contributing. More information can be found in the [Code of Conduct](https://github.com/elysiae-project/irmin/CODE_OF_CONDUCT.md)

### Dependencies and Rust/Meson versions

Generally, you are not allowed to add, update or remove crates or change rust versions except if:
1. You have a new feature that requires a new or more recent crate/rust version
2. You are writing a unit test that requires a new crate (must be added to dev-dependencies)
3. A version of a crate is out of support or has a major security vulnerability
 
If your contribution requires a rust/meson version change, you should update the relevant files (i.e. [rust-toolchain](https://github.com/elysiae-project/irmin/rust-toolchain) and [CONTRIBUTING.md](https://github.com/elysiae-project/irmin/CONTRIBUTING.md)) to reflect the new software requirements to build Elysiae

### Tests

You should always add unit tests to new functions you create, unless if they are very simple functions that you are certain cannot fail in any environment. As mentioned above, you are allowed to add rust crates (in dev-dependencies) and update the rust version if the crate requires a newer rust version

### Code Comments

Comments should reflect your understanding and explanations, rather than generic AI generated comments. Generic comments that state the obvious should be removed, rather than left in the code. Comments not in compliance with this policy will not cause your contribution to be rejected, but you may be asked to revise them if they are inappropriate.

### Commit Messages

Commit messages may be generated with AI assistance, so long as they are accurate description of the changes they describe.

### Pull Requests

Pull request descriptions must be written by the contributor, and should reflect their understanding and investigation of the changes requested.

### Issues

Issues must be written by the contributor, and should reflect their understanding and investigation of the issue.

### Review

Maintainers may ask you to explain any part of your contribution.

Your inability to adequately explain a change you submitted may be construed as failure to meet your responsibilities as a contributor, regardless of whether you used AI assisted development tools.
