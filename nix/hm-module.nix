{ config, pkgs, lib, ... }:

let
  inherit (lib) types mkIf mkEnableOption mkOption literalExpression
                 filterAttrs;
  toml = pkgs.formats.toml { };

  # Build a TOML section; returns null when all values are null (omitted from output)
  mkSection = v: let f = filterAttrs (_: v: v != null) v; in if f == {} then null else f;

  cfg = config.programs.driftwm;

  # ── Build TOML-compatible attrset from typed options ──

  kbd = mkSection {
    layout                        = cfg.input.keyboard.layout;
    remember_layout_per_window    = cfg.input.keyboard.rememberLayoutPerWindow;
    variant                       = cfg.input.keyboard.variant;
    options                       = cfg.input.keyboard.options;
    model                         = cfg.input.keyboard.model;
    repeat_rate                   = cfg.input.keyboard.repeatRate;
    repeat_delay                  = cfg.input.keyboard.repeatDelay;
    layout_independent            = cfg.input.keyboard.layoutIndependent;
    num_lock                      = cfg.input.keyboard.numLock;
    caps_lock                     = cfg.input.keyboard.capsLock;
  };

  tpad = mkSection {
    tap_to_click                  = cfg.input.trackpad.tapToClick;
    natural_scroll                = cfg.input.trackpad.naturalScroll;
    tap_and_drag                  = cfg.input.trackpad.tapAndDrag;
    accel_speed                   = cfg.input.trackpad.accelSpeed;
    accel_profile                 = cfg.input.trackpad.accelProfile;
    click_method                  = cfg.input.trackpad.clickMethod;
    disable_while_typing          = cfg.input.trackpad.disableWhileTyping;
  };

  mse = mkSection {
    accel_speed                   = cfg.input.mouse.accelSpeed;
    accel_profile                 = cfg.input.mouse.accelProfile;
    natural_scroll                = cfg.input.mouse.naturalScroll;
  };

  cur = mkSection {
    theme                         = cfg.cursor.theme;
    size                          = cfg.cursor.size;
    inactive_opacity              = cfg.cursor.inactiveOpacity;
  };

  nav = mkSection {
    trackpad_speed                = cfg.navigation.trackpadSpeed;
    mouse_speed                   = cfg.navigation.mouseSpeed;
    drift                         = cfg.navigation.drift;
    animation_speed               = cfg.navigation.animationSpeed;
    auto_navigate_on_close        = cfg.navigation.autoNavigateOnClose;
    nudge_step                    = cfg.navigation.nudgeStep;
    pan_step                      = cfg.navigation.panStep;
    anchors                       = if cfg.navigation.anchors != [] then cfg.navigation.anchors else null;
    edge_pan                      = mkSection {
      zone                        = cfg.navigation.edgePan.zone;
      speed_min                   = cfg.navigation.edgePan.speedMin;
      speed_max                   = cfg.navigation.edgePan.speedMax;
      cursor_pan                  = cfg.navigation.edgePan.cursorPan;
      cursor_zone                 = cfg.navigation.edgePan.cursorZone;
    };
  };

  zm = mkSection {
    step                          = cfg.zoom.step;
    fit_padding                   = cfg.zoom.fitPadding;
    reset_on_new_window           = cfg.zoom.resetOnNewWindow;
    reset_on_activation           = cfg.zoom.resetOnActivation;
  };

  snp = mkSection {
    enabled                       = cfg.snap.enabled;
    gap                           = cfg.snap.gap;
    distance                      = cfg.snap.distance;
    break_force                   = cfg.snap.breakForce;
    corners                       = cfg.snap.corners;
    centers                       = cfg.snap.centers;
  };

  dec = mkSection {
    bg_color                      = cfg.decorations.bgColor;
    fg_color                      = cfg.decorations.fgColor;
    corner_radius                 = cfg.decorations.cornerRadius;
    shadow                        = cfg.decorations.shadow;
    title_bar_height              = cfg.decorations.titleBarHeight;
    font                          = cfg.decorations.font;
    font_size                     = cfg.decorations.fontSize;
    font_weight                   = cfg.decorations.fontWeight;
    title_align                   = cfg.decorations.titleAlign;
    default_mode                  = cfg.decorations.defaultMode;
    border_width                  = cfg.decorations.borderWidth;
    border_color                  = cfg.decorations.borderColor;
    border_color_focused          = cfg.decorations.borderColorFocused;
  };

  eff = mkSection {
    blur_radius                   = cfg.effects.blurRadius;
    blur_strength                 = cfg.effects.blurStrength;
    animate_blur                  = cfg.effects.animateBlur;
  };

  bg = mkSection {
    type                          = cfg.background.type;
    path                          = cfg.background.path;
    texture                       = cfg.background.texture;
    mirror_tile                   = cfg.background.mirrorTile;
    cache_shader                  = cfg.background.cacheShader;
    transparent_shader            = cfg.background.transparentShader;
    cache_budget_mb               = cfg.background.cacheBudgetMb;
  };

  xwl = mkSection {
    enabled                       = cfg.xwayland.enabled;
    path                          = cfg.xwayland.path;
  };

  bck = mkSection {
    wait_for_frame_completion     = cfg.backend.waitForFrameCompletion;
    disable_direct_scanout        = cfg.backend.disableDirectScanout;
    disable_hardware_cursor       = cfg.backend.disableHardwareCursor;
    max_capture_fps               = cfg.backend.maxCaptureFps;
  };

  oul = mkSection {
    color                         = cfg.output.outline.color;
    thickness                     = cfg.output.outline.thickness;
    opacity                       = cfg.output.outline.opacity;
  };

  mseTab = let
    ow = if cfg.mouse.onWindow != {} then cfg.mouse.onWindow else null;
    oc = if cfg.mouse.onCanvas != {} then cfg.mouse.onCanvas else null;
    aw = if cfg.mouse.anywhere != {} then cfg.mouse.anywhere else null;
  in mkSection {
    resize_on_border              = cfg.mouse.resizeOnBorder;
    decoration_resize_snapped     = cfg.mouse.decorationResizeSnapped;
    decoration_fit_snapped        = cfg.mouse.decorationFitSnapped;
    "on-window"                   = ow;
    "on-canvas"                   = oc;
    anywhere                      = aw;
  };

  gst = let
    gw = if cfg.gestures.onWindow != {} then cfg.gestures.onWindow else null;
    gc = if cfg.gestures.onCanvas != {} then cfg.gestures.onCanvas else null;
    ga = if cfg.gestures.anywhere != {} then cfg.gestures.anywhere else null;
  in mkSection {
    swipe_threshold               = cfg.gestures.swipeThreshold;
    pinch_in_threshold            = cfg.gestures.pinchInThreshold;
    pinch_out_threshold           = cfg.gestures.pinchOutThreshold;
    "on-window"                   = gw;
    "on-canvas"                   = gc;
    anywhere                      = ga;
  };

  driftwmConfig = filterAttrs (_: v: v != null) {
    mod_key                       = cfg.modKey;
    focus_follows_mouse           = cfg.focusFollowsMouse;
    window_placement              = cfg.windowPlacement;
    env                           = if cfg.env != {} then cfg.env else null;
    autostart                     = if cfg.autostart != [] then cfg.autostart else null;
    input                         = mkSection { keyboard = kbd; trackpad = tpad; mouse = mse; };
    cursor                        = cur;
    navigation                    = nav;
    zoom                          = zm;
    snap                          = snp;
    decorations                   = dec;
    effects                       = eff;
    background                    = bg;
    xwayland                      = xwl;
    backend                       = bck;
    output                        = mkSection { outline = oul; };
    outputs                       = if cfg.outputs != [] then cfg.outputs else null;
    window_rules                  = if cfg.windowRules != [] then cfg.windowRules else null;
    keybindings                   = if cfg.keybindings != {} then cfg.keybindings else null;
    mouse                         = mseTab;
    gestures                      = gst;
  };

in {
  options.programs.driftwm = {

    enable = mkEnableOption "driftwm";

    package = mkOption {
      type = types.package;
      default = pkgs.driftwm;
      defaultText = literalExpression "pkgs.driftwm";
      description = "driftwm package to install";
    };

    # ── General ──
    modKey = mkOption {
      type = types.nullOr (types.enum [ "super" "alt" ]);
      default = null;
      description = "Modifier key: super or alt";
    };

    focusFollowsMouse = mkOption {
      type = types.nullOr types.bool;
      default = null;
      description = "Sloppy focus: keyboard follows pointer";
    };

    windowPlacement = mkOption {
      type = types.nullOr (types.enum [ "center" "cursor" "auto" ]);
      default = null;
      description = "Where new windows spawn";
    };

    autostart = mkOption {
      type = types.nullOr (types.listOf types.str);
      default = null;
      description = "Commands to run at startup";
    };

    env = mkOption {
      type = types.nullOr (types.attrsOf types.str);
      default = null;
      description = "Environment variables for child processes";
    };

    # ── Input ──
    input = {

      keyboard = {
        layout = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "XKB layout (e.g. us, ru)";
        };

        rememberLayoutPerWindow = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Remember layout per window";
        };

        variant = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "XKB variant (e.g. dvorak)";
        };

        options = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "XKB options (e.g. grp:win_space_toggle)";
        };

        model = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "XKB model (e.g. pc105)";
        };

        repeatRate = mkOption {
          type = types.nullOr types.int;
          default = null;
          description = "Key repeat rate (keys/sec)";
        };

        repeatDelay = mkOption {
          type = types.nullOr types.int;
          default = null;
          description = "Key repeat delay (ms)";
        };

        layoutIndependent = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Match bindings by physical key position";
        };

        numLock = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Num lock state on startup";
        };

        capsLock = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Caps lock state on startup";
        };
      };

      trackpad = {
        tapToClick = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Enable tap-to-click";
        };

        naturalScroll = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Reverse scroll direction";
        };

        tapAndDrag = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Double-tap-hold to drag";
        };

        accelSpeed = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Pointer acceleration (-1.0 to 1.0)";
        };

        accelProfile = mkOption {
          type = types.nullOr (types.enum [ "flat" "adaptive" ]);
          default = null;
          description = "Acceleration profile";
        };

        clickMethod = mkOption {
          type = types.nullOr (types.enum [ "none" "clickfinger" "button_areas" ]);
          default = null;
          description = "Click method";
        };

        disableWhileTyping = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Ignore trackpad shortly after key press";
        };
      };

      mouse = {
        accelSpeed = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Pointer acceleration (-1.0 to 1.0)";
        };

        accelProfile = mkOption {
          type = types.nullOr (types.enum [ "flat" "adaptive" ]);
          default = null;
          description = "Acceleration profile";
        };

        naturalScroll = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Reverse scroll direction";
        };
      };

    };

    # ── Cursor ──
    cursor = {
      theme = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "XCURSOR_THEME";
      };

      size = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "XCURSOR_SIZE";
      };

      inactiveOpacity = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Cursor opacity on non-active outputs (0.0-1.0)";
      };
    };

    # ── Navigation ──
    navigation = {
      trackpadSpeed = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Trackpad pan multiplier";
      };

      mouseSpeed = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Mouse drag pan multiplier";
      };

      drift = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Momentum coast (0 = off, 1 = floatiest)";
      };

      animationSpeed = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Camera lerp factor (higher = faster)";
      };

      autoNavigateOnClose = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Pan to focused window on close";
      };

      nudgeStep = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Px per nudge-window action";
      };

      panStep = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Px per pan-viewport action";
      };

      anchors = mkOption {
        type = types.nullOr (types.listOf (types.listOf types.float));
        default = null;
        description = "Canvas anchors [[x, y], ...]";
      };

      edgePan = {
        zone = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Edge-pan activation zone (px)";
        };

        speedMin = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Px/frame at zone boundary";
        };

        speedMax = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Px/frame at viewport edge";
        };

        cursorPan = mkOption {
          type = types.nullOr types.bool;
          default = null;
          description = "Pan when cursor touches screen edge";
        };

        cursorZone = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Cursor edge-pan activation zone (px)";
        };
      };
    };

    # ── Zoom ──
    zoom = {
      step = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Multiplier per keypress";
      };

      fitPadding = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Viewport px padding for zoom-to-fit";
      };

      resetOnNewWindow = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Animate zoom to 1.0 on new window";
      };

      resetOnActivation = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Animate zoom to 1.0 on off-screen focus";
      };
    };

    # ── Snap ──
    snap = {
      enabled = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Magnetic edge snapping";
      };

      gap = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Gap between snapped windows (canvas px)";
      };

      distance = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Activation threshold (screen px)";
      };

      breakForce = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Screen px past snap to break free";
      };

      corners = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Also align corners";
      };

      centers = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Also align centers";
      };
    };

    # ── Decorations ──
    decorations = {
      bgColor = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Title bar background color";
      };

      fgColor = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Title text color";
      };

      cornerRadius = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Window corner clip radius";
      };

      shadow = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Drop shadow under window chrome";
      };

      titleBarHeight = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "SSD title bar height (px)";
      };

      font = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Title text font family";
      };

      fontSize = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Title text size (points)";
      };

      fontWeight = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Title text weight";
      };

      titleAlign = mkOption {
        type = types.nullOr (types.enum [ "left" "center" ]);
        default = null;
        description = "Title text alignment";
      };

      defaultMode = mkOption {
        type = types.nullOr (types.enum [ "client" "minimal" "none" ]);
        default = null;
        description = "Default decoration mode";
      };

      borderWidth = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Border width (px, 0 disables)";
      };

      borderColor = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Unfocused border color";
      };

      borderColorFocused = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Focused border color";
      };
    };

    # ── Effects ──
    effects = {
      blurRadius = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Kawase blur passes";
      };

      blurStrength = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Per-pass texel spread";
      };

      animateBlur = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Re-blur every frame with animated wallpaper";
      };
    };

    # ── Background ──
    background = {
      type = mkOption {
        type = types.nullOr (types.enum [ "default" "shader" "tile" "wallpaper" "none" ]);
        default = null;
        description = "Background type";
      };

      path = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Path to shader/tile/wallpaper";
      };

      texture = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Texture path for shader background";
      };

      mirrorTile = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Mirror-fold tile image";
      };

      cacheShader = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Cache static shader to texture";
      };

      transparentShader = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Honor shader output alpha";
      };

      cacheBudgetMb = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Shader/TIFF cache budget (MB)";
      };
    };

    # ── Keybindings ──
    keybindings = mkOption {
      type = types.nullOr (types.attrsOf types.str);
      default = null;
      description = "Keyboard bindings (\"key\" = \"action\")";
    };

    # ── Mouse ──
    mouse = {
      resizeOnBorder = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Enable resize via window border drag";
      };

      decorationResizeSnapped = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Border resize propagates to snap cluster";
      };

      decorationFitSnapped = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Decoration fit propagates to snap cluster";
      };

      onWindow = mkOption {
        type = types.nullOr (types.attrsOf types.str);
        default = null;
        description = "Mouse bindings on-window";
      };

      onCanvas = mkOption {
        type = types.nullOr (types.attrsOf types.str);
        default = null;
        description = "Mouse bindings on-canvas";
      };

      anywhere = mkOption {
        type = types.nullOr (types.attrsOf types.str);
        default = null;
        description = "Mouse bindings anywhere";
      };
    };

    # ── Gestures ──
    gestures = {
      swipeThreshold = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Swipe threshold (px)";
      };

      pinchInThreshold = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Pinch-in scale threshold";
      };

      pinchOutThreshold = mkOption {
        type = types.nullOr types.float;
        default = null;
        description = "Pinch-out scale threshold";
      };

      onWindow = mkOption {
        type = types.nullOr (types.attrsOf types.str);
        default = null;
        description = "Gesture bindings on-window";
      };

      onCanvas = mkOption {
        type = types.nullOr (types.attrsOf types.str);
        default = null;
        description = "Gesture bindings on-canvas";
      };

      anywhere = mkOption {
        type = types.nullOr (types.attrsOf types.str);
        default = null;
        description = "Gesture bindings anywhere";
      };
    };

    # ── XWayland ──
    xwayland = {
      enabled = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Enable xwayland-satellite support";
      };

      path = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Path to xwayland-satellite binary";
      };
    };

    # ── Backend ──
    backend = {
      waitForFrameCompletion = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Force GPU-fence wait before every page flip";
      };

      disableDirectScanout = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Force EGL composition";
      };

      disableHardwareCursor = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Composite cursor into frame";
      };

      maxCaptureFps = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Cap FPS for screen capture clients (0 = unlimited)";
      };
    };

    # ── Output Outline ──
    output = {
      outline = {
        color = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Monitor viewport outline color";
        };

        thickness = mkOption {
          type = types.nullOr types.int;
          default = null;
          description = "Outline thickness (px, 0 disables)";
        };

        opacity = mkOption {
          type = types.nullOr types.float;
          default = null;
          description = "Outline opacity (0.0-1.0)";
        };
      };
    };

    # ── Outputs (array of tables) ──
    outputs = mkOption {
      type = types.nullOr (types.listOf (types.submodule {
        options = {
          name = mkOption {
            type = types.str;
            description = "Connector name (required)";
          };
          scale = mkOption {
            type = types.nullOr types.float;
            default = null;
            description = "Fractional scale";
          };
          transform = mkOption {
            type = types.nullOr (types.enum [
              "normal" "90" "180" "270"
              "flipped" "flipped-90" "flipped-180" "flipped-270"
            ]);
            default = null;
            description = "Output transform";
          };
          position = mkOption {
            type = types.nullOr (types.either (types.enum [ "auto" ]) (types.listOf types.int));
            default = null;
            description = "Output position";
          };
          mode = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Output mode (e.g. preferred, 1920x1080@60)";
          };
        };
      }));
      default = null;
      description = "Per-output configuration";
    };

    # ── Window Rules (array of tables) ──
    windowRules = mkOption {
      type = types.nullOr (types.listOf (types.submodule {
        options = {
          appId = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Match by app_id";
          };
          title = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Match by window title";
          };
          position = mkOption {
            type = types.nullOr (types.listOf types.int);
            default = null;
            description = "Window position [x, y]";
          };
          size = mkOption {
            type = types.nullOr (types.listOf types.int);
            default = null;
            description = "Window size [w, h]";
          };
          widget = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Pin as widget (immovable, below normal)";
          };
          pinnedToScreen = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Lock window to screen space";
          };
          decoration = mkOption {
            type = types.nullOr (types.enum [ "client" "server" "minimal" "none" ]);
            default = null;
            description = "Override decoration mode";
          };
          blur = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Blur background behind window";
          };
          opacity = mkOption {
            type = types.nullOr types.float;
            default = null;
            description = "Window opacity (0.0-1.0)";
          };
          borderWidth = mkOption {
            type = types.nullOr types.int;
            default = null;
            description = "Per-window border width";
          };
          borderColor = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Per-window unfocused border color";
          };
          borderColorFocused = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Per-window focused border color";
          };
          cornerRadius = mkOption {
            type = types.nullOr types.int;
            default = null;
            description = "Per-window corner radius";
          };
          shadow = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Per-window shadow toggle";
          };
          passKeys = mkOption {
            type = types.nullOr (types.either types.bool (types.listOf types.str));
            default = null;
            description = "Key pass-through (true=all, []=specific combos)";
          };
        };
      }));
      default = null;
      description = "Window rules";
    };

  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."driftwm/config.toml".source =
      toml.generate "driftwm-config" driftwmConfig;
  };
}
