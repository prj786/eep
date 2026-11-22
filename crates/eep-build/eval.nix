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
      # A renamed or removed attribute is often an alias that THROWS when
      # touched: caught here, so one stale catalog entry never fails the
      # whole release (only that app is marked unavailable).
      look = builtins.tryEval (lib.attrByPath (lib.splitString "." n) null p);
      found = if look.success then look.value else null;
      d = found;
      licensed = builtins.tryEval (
        lib.all (l: !(builtins.isAttrs l) || (l.free or true)) (lib.toList (d.meta.license or [ ]))
      );
      free = licensed.success && licensed.value;
      r = builtins.tryEval (builtins.seq d.outPath true);
    in
    if found == null then { error = if look.success then "no such attribute" else "the attribute fails to evaluate (renamed or removed?)"; }
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
