# Inkult — Claude Code Instructions

## Project Overview

**Inkult** is a self-hosted music downloader and library manager built for DJs and music enthusiasts.

It provides a modern UI inspired by Spotify while supporting music acquisition from multiple platforms such as:

* YouTube
* Spotify
* Deezer
* SoundCloud
* Other supported music sources

Inkult focuses on obtaining the highest-quality available audio, preserving rich track metadata, organizing downloaded files into a useful music-library structure, and converting files between supported audio formats.

**Language:** Rust

---

## Core Goals

When working on Inkult, prioritize:

1. **Audio quality** — Always prefer the best available source and highest-quality audio.
2. **Metadata quality** — Preserve as much useful track metadata as possible.
3. **Reliable downloads** — Downloads should be resumable, observable, and robust against failures.
4. **Library organization** — Files should be automatically organized into a consistent, configurable directory structure.
5. **Excellent UX** — The application should feel familiar to users of modern music streaming applications.
6. **Self-hosting** — Inkult should be easy to deploy and operate without requiring external infrastructure beyond the supported music sources.
7. **Extensibility** — New sources, formats, metadata providers, and output formats should be easy to add.
8. **Safety and correctness** — Avoid corrupting files, losing metadata, overwriting user music unexpectedly, or silently failing.

---

# Development Principles

## Rust

Use idiomatic, modern Rust.

Prefer:

* Strong types over loosely typed data.
* `Result` and `Option` over panics for expected failures.
* Explicit error types for domain-level errors.
* Small, composable functions.
* Clear ownership boundaries.
* Async code where I/O benefits from concurrency.
* Traits when they provide a meaningful abstraction.
* Iterators and standard-library functionality when they improve readability.

Avoid:

* `unwrap()` / `expect()` in production code unless failure is genuinely impossible and the invariant is documented.
* Excessive cloning.
* Global mutable state.
* Large functions with many responsibilities.
* Premature abstractions.
* Stringly-typed representations when an enum or dedicated type would be clearer.

Run formatting and linting before considering Rust changes complete:

```bash
cargo fmt
cargo clippy
cargo test
```

If the project uses additional checks or CI commands, follow those instead.

---

# Architecture

Keep the project modular.

The application should conceptually separate:

```text
Sources
   │
   ▼
Resolver / Downloader
   │
   ▼
Track Metadata
   │
   ├──► Metadata Enrichment
   │
   ├──► Artwork
   │
   └──► Quality Selection
   │
   ▼
Audio Processing / Conversion
   │
   ▼
Library Organizer
   │
   ▼
Filesystem
```

The UI should communicate with the application through well-defined application/domain interfaces rather than directly implementing downloading or filesystem logic.

Avoid coupling source-specific implementations to the rest of the application.

---

# Music Sources

Each music source should be treated as an independent provider.

A provider should ideally be responsible for:

* URL recognition.
* Track/album/playlist resolution.
* Metadata extraction.
* Available media/quality discovery.
* Downloading or delegating the download.
* Provider-specific limitations and errors.

Do not spread provider-specific logic throughout the application.

Prefer a common abstraction such as:

```rust
trait MusicProvider {
    // Resolve a URL or identifier into domain objects.
}
```

The exact trait/interface should match the existing architecture rather than being introduced unnecessarily.

### Adding a New Provider

When adding a new provider:

1. Keep provider-specific behavior isolated.
2. Reuse existing domain types.
3. Preserve the same metadata semantics used by other providers.
4. Expose provider-specific limitations through explicit errors or capabilities.
5. Add tests for URL recognition and metadata extraction where practical.
6. Do not duplicate common downloading, conversion, or organization logic.

---

# Audio Quality

Audio quality is one of Inkult's primary features.

The application should:

* Prefer lossless audio when the source actually provides it.
* Otherwise select the highest-quality available lossy source.
* Avoid unnecessary transcoding.
* Preserve the original downloaded format when appropriate.
* Never claim a file is lossless if the source was already lossy.
* Avoid transcoding lossy audio into another lossy format unless explicitly requested.

When converting formats, clearly distinguish:

```text
Source quality
    ↓
Downloaded quality
    ↓
Converted/output quality
```

A conversion cannot restore information that was lost in the original source.

Do not optimize solely for bitrate. Consider:

* Codec.
* Bit depth.
* Sample rate.
* Container.
* Source availability.
* Whether the source itself is lossless or lossy.

---

# Metadata

Metadata is a first-class part of Inkult.

Where available, preserve or retrieve:

* Title
* Artists
* Album
* Album artist
* Track number
* Disc number
* Release date
* Genre
* Description
* Copyright
* Publisher / label
* Source URL
* Provider
* Artwork / cover
* Playlist information
* Additional provider metadata

Use structured domain types instead of passing raw metadata maps throughout the application.

Metadata from different providers may use different names or formats. Normalize it into Inkult's internal representation.

### Metadata precedence

When metadata comes from multiple sources, use a deterministic precedence strategy.

For example:

```text
User-provided metadata
        ↓
Trusted metadata provider
        ↓
Source/provider metadata
        ↓
Inferred metadata
```

Do not silently overwrite higher-quality metadata with lower-quality metadata.

---

# Artwork

Artwork should be treated as metadata and preserved whenever available.

Prefer:

1. Highest-resolution artwork available.
2. Original artwork rather than generated thumbnails.
3. Embedded artwork in the output file when the format supports it.
4. A sensible library-level artwork file when embedding is impossible.

Avoid unnecessarily downloading the same artwork multiple times.

---

# File Organization

Downloaded music should be automatically organized into a predictable directory hierarchy.

The default organization should favor a structure familiar to DJs and music-library users.

A good default is:

```text
Music/
└── Artist/
    └── Album/
        └── 01 - Track Title.ext
```

For singles or tracks without an album:

```text
Music/
└── Artist/
    └── Singles/
        └── Track Title.ext
```

For compilation albums:

```text
Music/
└── Various Artists/
    └── Album/
        └── 01 - Artist - Track Title.ext
```

The exact structure should be configurable where practical.

### Filename safety

Never use raw provider metadata directly as a filename.

Sanitize:

* Path separators.
* Reserved filesystem characters.
* Control characters.
* Invalid Unicode sequences.
* Platform-specific reserved names.
* Excessively long filenames.

Avoid path traversal vulnerabilities.

Metadata such as:

```text
../../somewhere
```

must never be able to escape the configured music directory.

---

# Duplicate Handling

Downloading the same track twice should not unexpectedly create multiple copies.

Consider tracks equivalent using stable identifiers when available.

Potential identity sources include:

* Provider track ID.
* Canonical source URL.
* ISRC.
* Other stable external identifiers.

Do not rely exclusively on title + artist because different recordings can share those fields.

When duplicate detection is uncertain, prefer preserving user data over deleting or overwriting files.

---

# File Conversion

Inkult includes an embedded file-format converter.

Conversion should:

* Support the formats actually provided by the application.
* Preserve metadata whenever possible.
* Preserve artwork whenever the target container supports it.
* Clearly report conversion failures.
* Never delete the source file before successful conversion.
* Avoid unnecessary conversions.

A conversion operation should be treated as:

```text
Source file
    │
    ├── Read metadata
    │
    ├── Decode/transcode
    │
    ├── Write temporary output
    │
    ├── Verify output
    │
    ├── Apply metadata/artwork
    │
    └── Atomically move into final location
```

Use temporary files to avoid leaving partially written music files after a crash.

---

# Downloads

Downloads should be resilient.

Handle:

* Network failures.
* Provider failures.
* Timeouts.
* Rate limits.
* Interrupted downloads.
* Invalid media.
* Disk-full errors.
* Permission errors.
* Conversion failures.

A failed download should provide a useful error to the user.

Avoid silently retrying indefinitely.

Retries should have:

* A reasonable maximum.
* Appropriate backoff.
* Clear distinction between retryable and permanent failures.

---

# Concurrency

Downloads and processing may run concurrently.

However:

* Do not overwhelm providers.
* Do not consume unbounded memory.
* Do not create unlimited concurrent filesystem operations.
* Respect configured concurrency limits.
* Ensure jobs can be cancelled cleanly.

Prefer bounded queues and explicit job states.

A download job should have a clear lifecycle, for example:

```text
Queued
  ↓
Resolving
  ↓
Downloading
  ↓
Processing
  ↓
Organizing
  ↓
Completed
```

With failure/cancellation paths:

```text
Downloading ──► Failed
Downloading ──► Cancelled
Processing  ──► Failed
```

---

# UI / UX

The UI should feel similar in spirit to modern music streaming applications without unnecessarily copying proprietary designs.

Prioritize:

* Search.
* Track/album/playlist views.
* Download queues.
* Download progress.
* Library browsing.
* Metadata visibility.
* Format/conversion controls.
* Clear error states.
* Responsive interactions.

Long-running operations should never block the UI.

Users should be able to understand:

* What is downloading.
* Current progress.
* Which source is being used.
* Selected quality.
* Where the resulting file will be stored.
* Whether metadata/artwork were successfully retrieved.
* Why a download failed.

---

# API / Domain Boundaries

Keep domain logic independent from presentation concerns.

Prefer:

```text
UI
 │
 ▼
Application Services
 │
 ▼
Domain
 │
 ├── Providers
 ├── Metadata
 ├── Downloads
 ├── Conversion
 └── Library
```

Avoid:

```text
UI
 ├── Direct filesystem manipulation
 ├── Direct provider API calls
 ├── Direct ffmpeg invocation
 └── Metadata parsing
```

Business logic should be testable without requiring the UI.

---

# Error Handling

Errors should provide useful context.

Prefer errors such as:

```text
Failed to download track:
  provider: YouTube
  track: Example Track
  reason: network timeout
```

over generic errors such as:

```text
download failed
```

Use typed errors where the caller can reasonably react differently to different failures.

Errors should preserve their underlying causes when useful.

Never swallow errors silently.

---

# Logging

Use structured logging where supported.

Logs should make it possible to diagnose:

* Which track was being processed.
* Which provider was used.
* Which operation failed.
* Whether a retry occurred.
* Which stage failed.
* Relevant error causes.

Do not log:

* Authentication tokens.
* API secrets.
* Passwords.
* Private user data unnecessarily.
* Full sensitive URLs containing credentials.

Use appropriate log levels:

```text
trace/debug → development details
info        → normal application events
warn        → recoverable/unexpected conditions
error       → failed operations
```

---

# Configuration

Configuration should be explicit and validated.

Potential configuration includes:

* Music library directory.
* Download directory.
* Preferred audio format.
* Preferred quality.
* Filename/path templates.
* Maximum concurrent downloads.
* Retry settings.
* Provider configuration.
* Conversion settings.

Invalid configuration should produce a clear startup/configuration error.

Avoid hidden configuration that changes application behavior unexpectedly.

---

# Testing

Write tests for important domain behavior.

Prioritize tests around:

* Provider URL parsing.
* Metadata normalization.
* Quality selection.
* Filename sanitization.
* Directory organization.
* Duplicate detection.
* Conversion behavior.
* Error handling.
* Configuration validation.

Filesystem-heavy functionality should use temporary directories in tests.

Avoid tests that depend on external services unless they are explicitly integration tests.

External-provider tests should not be required for the normal unit-test suite when avoidable.

---

# Security

Inkult is self-hosted and handles URLs, metadata, downloaded files, and filesystem paths.

Treat all provider data as untrusted input.

Pay particular attention to:

* Path traversal.
* Malicious filenames.
* Malformed metadata.
* Extremely large metadata fields.
* Malicious or malformed media files.
* Command injection when invoking external tools.
* Unsafe shell argument construction.
* SSRF if arbitrary URLs can be fetched.
* Resource exhaustion.
* Temporary-file handling.

Never construct shell commands by concatenating untrusted strings.

Prefer direct process APIs with separate arguments.

---

# External Tools

If Inkult relies on external tools such as media downloaders, FFmpeg, or similar utilities:

* Detect whether the dependency is available.
* Validate versions/capabilities where necessary.
* Invoke processes without a shell when possible.
* Capture stdout/stderr appropriately.
* Surface meaningful errors.
* Do not assume a particular installation path.
* Keep external-tool integration behind a dedicated abstraction.

---

# Dependency Management

Before adding a dependency, consider whether the functionality can reasonably be implemented with:

1. The Rust standard library.
2. An existing project dependency.
3. A small, well-maintained crate.

Avoid adding dependencies for trivial functionality.

When adding a dependency:

* Prefer actively maintained crates.
* Check licensing compatibility.
* Avoid unnecessary feature flags.
* Keep dependency scope minimal.

---

# Code Changes

When modifying the project:

1. Understand the existing architecture before introducing new abstractions.
2. Prefer small, focused changes.
3. Reuse existing types and utilities.
4. Do not rewrite working code without a clear reason.
5. Keep unrelated refactoring out of feature changes.
6. Update tests when behavior changes.
7. Update documentation when user-facing behavior changes.
8. Run formatting, linting, and tests.

Before completing a task, check:

```bash
cargo fmt --check
cargo clippy
cargo test
```

If any command cannot be run because of the local environment, clearly report that instead of claiming the change was verified.

---

# Git Hygiene

Keep commits and changes focused.

Do not:

* Modify unrelated files.
* Commit generated build artifacts.
* Commit downloaded music.
* Commit secrets or API credentials.
* Rewrite history unless explicitly requested.
* Remove user data as part of a development task.

Before making destructive filesystem changes, verify the target path carefully.

---

# Documentation

Document behavior that future contributors would not reasonably infer from the code.

Especially document:

* Non-obvious provider behavior.
* Quality-selection decisions.
* Metadata precedence.
* File-organization rules.
* External-tool requirements.
* Important security assumptions.
* Compatibility constraints.

Avoid comments that merely restate the code.

Prefer explaining **why** something is implemented a particular way.

---

# Product Philosophy

Inkult should feel like a tool built by someone who actually manages a large music library.

When making product decisions, favor:

* Predictability over magic.
* Data preservation over destructive cleanup.
* High-quality audio over convenience.
* Rich metadata over minimal metadata.
* Clear feedback over silent automation.
* Configurability where users have genuinely different workflows.
* Sensible defaults so configuration is not required for normal use.

The application should make it easy to go from:

```text
"I found this track"
        ↓
"Download it"
        ↓
"Get the best available quality"
        ↓
"Get the correct metadata and artwork"
        ↓
"Convert if necessary"
        ↓
"Put it in the right place"
        ↓
"Have a clean, usable music library"
```

with as little manual work as possible.

---

# Important Implementation Rule

Do not implement features solely according to the wording of a ticket or prompt if doing so would violate the project's core principles.

Before implementing a change, consider:

* Does this preserve audio quality?
* Does this preserve metadata?
* Is the behavior safe for existing user files?
* Does it work with multiple providers?
* Does it belong in the correct architectural layer?
* Is the behavior testable?
* Does it remain extensible?
* Is the UX understandable?

When there is a tradeoff, favor correctness, data preservation, and maintainability over a quick implementation.
