# Evaluated once per release by `eep-build index` — in the nixos/nix
# container, or with a native Nix. Never on a client.
#
# Input (args.json): { "url": "<release>/nixexprs.tar.xz", "attrs": [ "firefox", … ] }
# Output: { apps.<attr> = { out, version, … } or { error }; driver = [ … ]; }
{ url, attrs }:
let
  p = import (fetchTarball url) { config.allowUnfree = false; };
  lib = p.lib;
  str = v: if builtins.isString v then v else "";
  first = v: if builtins.isList v then (if v == [ ] then "" else str (builtins.head v)) else str v;
  one = n:
    let
      found = lib.attrByPath (lib.splitString "." n) null p;
      d = found;
      licenses = lib.toList (d.meta.license or [ ]);
      free = lib.all (l: !(builtins.isAttrs l) || (l.free or true)) licenses;
      r = builtins.tryEval (builtins.seq d.outPath true);
    in
    if found == null then { error = "no such attribute"; }
    else if !free then { error = "unfree"; }
    else if !(d.meta.available or true) then { error = "not available on this platform"; }
    else if !r.success then { error = "failed to evaluate"; }
    else {
      out = d.outPath;
      pname = str (d.pname or "");
      version = str (d.version or "");
      mainProgram = str (d.meta.mainProgram or "");
      description = str (d.meta.description or "");
      homepage = first (d.meta.homepage or "");
    };
  # What NixOS links into /run/opengl-driver by default
  # (nixos/modules/hardware/graphics.nix: buildEnv [ hardware.graphics.package ],
  # which defaults to pkgs.mesa).
  outputsOf = d: map (o: d.${o}.outPath) (d.meta.outputsToInstall or [ "out" ]);
in
{
  apps = lib.genAttrs attrs one;
  driver = outputsOf p.mesa;
}
