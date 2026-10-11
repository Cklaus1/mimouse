# Nix Flake Usage

## Run

```bash
nix run github:Cklaus1/mimouse

# With params
nix run github:Cklaus1/mimouse -- --help

```

## Home-manager module

Add input:

```nix
inputs = {
    mimouse.url = "github:Cklaus1/mimouse";
}
```

Enable mimouse:

``` nix
{
  inputs,
  ...
}: {
  # Add the Home Manager module
  imports = [inputs.mimouse.homeManagerModules.default];

  programs.mimouse = {
    enable = true;
    # systemd = false;
    # package = inputs.mimouse.packages.${pkgs.stdenv.hostPlatform.system}.default
    # Optional configuration in nix syntax, see config.toml for available options
    # settings = { };
    };
  };
}

```
