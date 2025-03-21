# Roc installation guide for x86_64 macOS systems

## How to install Roc

:warning: We do not yet officially support MacOS 13. But, as long as you are not using a zig or wasm platform most things should work fine.

:warning: Macos x86_64 nightly releases lag behind the others due to [unresolved test failures](https://github.com/roc-lang/roc/issues/4655). The `dbg` keyword is not yet supported, as well as importing platforms using URLs. For the platform you can use the path instead, like in [this example](https://github.com/roc-lang/roc/blob/main/examples/helloWorld.roc).

In order to develop in Roc, you need to install the Roc CLI,
which includes the Roc compiler and various helpful utilities.

1. Download the latest nightly from the assets [here](https://github.com/roc-lang/roc/releases).

1. To prevent "roc can't be opened because Apple can't check it...":

    ```sh
    xattr -d com.apple.quarantine roc_nightly-macos_x86_64-<VERSION>.tar.gz
    ```

1. Untar the archive:

    ```sh
    mkdir roc_nightly-macos_x86_64-<VERSION>
    tar xf roc_nightly-macos_x86_64-<VERSION>.tar.gz
    cd roc_night<TAB TO AUTOCOMPLETE>
    ```

1. To be able to run the `roc` command anywhere on your system; add the line below to your shell startup script (.profile, .zshrc, ...):
    ```sh
    export PATH=$PATH:~/path/to/roc_nightly-macos_x86_64-<VERSION>
    ```

1. Check everything worked by executing `roc version`

## How to install Roc platform dependencies

This step is not necessary if you only want to use the [basic-cli platform](https://github.com/roc-lang/basic-cli), like in the tutorial.
But, if you want to compile Roc apps with other platforms (either in [`examples/`](https://github.com/roc-lang/roc/tree/main/examples) or in your own projects),
you'll need to install one or more of these platform languages too.

1. Install the Rust compiler, for apps with Rust-based platforms:

    ```sh
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    ```

1. If you'd like to use Zig-based platforms: download [zig 0.9.1](https://ziglang.org/download/0.9.1/zig-macos-x86_64-0.9.1.tar.xz), extract the archive and add `export PATH=$PATH:~/path/to/zig` to your shell startup script (.profile, .zshrc, …). Note: zig 0.9.1 is not available on homebrew.

1. Run examples:

    ```sh
    # Note: If you installed rust in this terminal session, you'll need to open a new one first!
    ./roc examples/platform-switching/rocLovesRust.roc

    ./roc examples/platform-switching/rocLovesZig.roc

    ./roc examples/platform-switching/rocLovesC.roc
    ```
