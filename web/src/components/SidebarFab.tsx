interface Props {
  sidebarOpen: boolean;
  onToggle: () => void;
}

// Optional mobile shortcut to the session sidebar, clustered with the
// keyboard FAB in the terminal pane so you don't have to reach the top-bar
// toggle mid-session (#2245). Off by default; enabled in Terminal settings.
export function SidebarFab({ sidebarOpen, onToggle }: Props) {
  return (
    <button
      type="button"
      aria-label={sidebarOpen ? "Close sidebar" : "Open sidebar"}
      onClick={onToggle}
      // Keep focus where it is: a button steals focus on pointer-down, which
      // would blur the terminal input and close the keyboard. The sidebar
      // should be openable while the keyboard is up. onClick still fires.
      onMouseDown={(e) => e.preventDefault()}
      className="absolute left-3 bottom-3 z-10 w-10 h-10 rounded-full bg-surface-800/90 border border-surface-700/30 text-text-secondary flex items-center justify-center shadow-lg backdrop-blur-sm active:scale-95"
    >
      <svg
        width="18"
        height="18"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden="true"
      >
        <rect x="3" y="4" width="18" height="16" rx="2" />
        <line x1="9" y1="4" x2="9" y2="20" />
      </svg>
    </button>
  );
}
