pragma Singleton
import Quickshell

// One popover at a time: opening one closes whichever is open.
Singleton {
    property var current: null
}
