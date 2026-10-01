# reman's question window (Linux): may an agent see a folder's command history? zenity (GNOME
# and most desktops) or kdialog (KDE), in the desktop's own theme. Prints the choice.
# Run by `reman mcp` (mcp.rs, window::ask) as:
#   sh -c "$this" sh <app> <folder> <session> <always> <no>
app=$1 folder=$2 session=$3 always=$4 no=$5
note="Secrets stay masked, and nothing leaves this computer. Allowing it for this session lasts until this chat ends; you'll be asked again next time."

# Pango markup for zenity: & < > in a name or a path must not read as markup
esc() { printf %s "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

if command -v zenity >/dev/null 2>&1; then
  text="<b><big>Share your command history?</big></b>

$(esc "$app") wants to see the commands you and your agents ran in this folder:

<tt>$(esc "$folder")</tt>

<small>$(esc "$note")</small>"
  # OK = this session; the extra button prints its own label; Cancel (or Esc) = no
  out=$(zenity --question --title=reman --width=480 --text="$text" \
    --ok-label="$session" --extra-button="$always" --cancel-label="$no")
  r=$?
  if [ $r -eq 0 ]; then printf %s "$session"
  elif [ -n "$out" ]; then printf %s "$out"
  elif [ $r -eq 1 ]; then printf %s "$no"
  fi
elif command -v kdialog >/dev/null 2>&1; then
  kdialog --title reman --yes-label "$session" --no-label "$always" --cancel-label "$no" \
    --yesnocancel "Share your command history?

$app wants to see the commands you and your agents ran in this folder:

$folder

$note"
  case $? in
    0) printf %s "$session" ;;
    1) printf %s "$always" ;;
    2) printf %s "$no" ;;
  esac
fi
