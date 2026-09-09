# RakuKrisOS

Minimal Fedora 44 bootc desktop OS derived from Fedora Minimal, retaining the RakuOS persistent `/usr` overlay and RUM package-management model.

## Design goals

- Fedora 44 Minimal as the base.
- Fedora stock kernel for the first release.
- bootc + OSTree/composefs + dracut + systemd.
- RakuOS persistent overlay infrastructure.
- RakuOS RUM as the overlay package manager.
- AppArmor as the only mandatory MAC; SELinux is not part of the runtime image.
- Flatpak support.
- KDE Plasma desktop with only the initial native GUI applications: Dolphin, Konsole and Kate.
- Keep Fedora and RakuOS repositories available.
- No gaming stack, NVIDIA-specific setup, Ollama, Nix or hardware-specific RakuOS extras in the first release.

## Repository layout

```text
.
├── Containerfile
├── build_files/
│   ├── base-packages.txt
│   └── desktop-packages.txt
└── packages/
    └── (RakuOS source components are tracked on rakuos-upstream while the
        slimmed variants are brought into the main build deliberately.)
```

## Branches

- `main`: RakuKrisOS build and configuration.
- `rakuos-upstream`: imported RakuOS components kept intact for comparison and provenance.

## Current status

The repository is intentionally being built in small, testable steps. The first implementation milestone is the Fedora 44 Minimal + RakuOS overlay boot path; desktop and package-selection work follows only after that path is verified.
