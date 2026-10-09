# DeckCraft for Android

The Gradle project that packages `apps/deckcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/deckcraft-android/src/lib.rs`: `android_main`, the platform services (open, save,
  preferences, links) and the JNI bridge to `MainActivity`. It mirrors the web shell
  (`apps/deckcraft-web`): picked files arrive as bytes through `Services::inbox` (presentations
  open, pictures/audio/video are inserted), saves and exports go out through `Services::download`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker, saving into
  `Downloads/DeckCraft/`, opening links, and the full-screen window.
- `cargo ndk` drops `libdeckcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p deckcraft-android
cd android && ./gradlew assembleDebug
```

Set `CRAFT_FONTS_DIR` to a checkout of storytold/craft-fonts to embed the shared fonts, as the
release builds do. System fonts are read from `/system/fonts`.

## Keyboard

Text boxes ask for the soft keyboard when they have focus; a Bluetooth or USB keyboard works
like on the desktop (Ctrl for the shortcuts; F5 / Shift+F5 start the slide show).

## Known limits (first version)

- No audio: media playback is clocked silently (the desktop's cpal output is not built for
  Android). Video shows its frames; audio clips and video soundtracks are muted.
- Save, Save As and Export (PDF/PNG/PPTX) write to `Downloads/DeckCraft/<name>` without a dialog;
  saving the same name again in one session overwrites it. There is no Save back into the opened
  file (SAF write-back).
- Open Recent is empty (picked files are names, not reopenable paths); no file drag and drop;
  no clipboard images (Paste of a picture from another app); no TCP control server.
- AutoRecover keeps unsaved presentations in the app's private storage and reopens them on the
  next launch.
- The PowerPoint-style ribbon needs a tablet-sized screen; on a phone's cover screen it is cramped.
