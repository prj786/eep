# eep: command-line apps installed with eep (eep install ripgrep → rg).
# Appended, so a pacman-installed command of the same name still wins.
_eep_bin="${XDG_DATA_HOME:-$HOME/.local/share}/eep/bin"
case ":$PATH:" in
    *":$_eep_bin:"*) ;;
    *) [ -n "$HOME" ] && PATH="$PATH:$_eep_bin" && export PATH ;;
esac
unset _eep_bin
