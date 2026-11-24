local IS_WINDOWS = package.config:sub(1, 1) == "\\"
local SHELL_FLAVOR = "com.superlogical.terminal.shell"
local FLOAT_LABEL = "hud-float"
local DOCK_LABEL = "hud-dock"
local TAB_LABEL = "hud-tab"

local OVERLAY_LABELS = {
  [FLOAT_LABEL] = true,
  [DOCK_LABEL] = true,
  [TAB_LABEL] = true,
  ["mdpeek-float"] = true,
  ["mdpeek-dock"] = true,
  ["omnidiff-float"] = true,
  ["omnidiff-dock"] = true,
  ["lazygit-float"] = true,
  ["scratch-float"] = true,
  ["btop-float"] = true,
}

local function file_exists(path)
  if not path or path == "" then
    return false
  end
  local f = io.open(path, "rb")
  if f then
    f:close()
    return true
  end
  return false
end

local function resolve_hud_binary()
  local explicit = os.getenv("REX_HUD_BIN")
  if file_exists(explicit) then
    return explicit
  end

  local home = os.getenv("HOME") or os.getenv("USERPROFILE") or ""
  local candidates = {}

  if IS_WINDOWS then
    if home ~= "" then
      table.insert(candidates, home .. "\\.local\\bin\\rex-hud.exe")
      table.insert(candidates, home .. "\\.cargo\\bin\\rex-hud.exe")
    end
  else
    if home ~= "" then
      table.insert(candidates, home .. "/.local/bin/rex-hud")
      table.insert(candidates, home .. "/.cargo/bin/rex-hud")
    end
    table.insert(candidates, "/home/linuxbrew/.linuxbrew/bin/rex-hud")
    table.insert(candidates, "/opt/homebrew/bin/rex-hud")
    table.insert(candidates, "/usr/local/bin/rex-hud")
  end

  for _, candidate in ipairs(candidates) do
    if file_exists(candidate) then
      return candidate
    end
  end

  return IS_WINDOWS and "rex-hud.exe" or "rex-hud"
end

local function resolve_active_window(view)
  if not view or not view.windows then
    return nil
  end
  for _, w in ipairs(view.windows) do
    if w.window_id == view.active_window_id then
      return w
    end
  end
  return view.windows[1]
end

local function resolve_block_cwd(block_id)
  if not block_id or block_id == "" then
    return nil
  end
  local proc = rex.block.call("com.superlogical.terminal", "process", {
    block_id = block_id,
  })
  if proc then
    if proc.foreground and proc.foreground.cwd and proc.foreground.cwd ~= "" then
      return proc.foreground.cwd
    end
    for _, anc in ipairs(proc.ancestors or {}) do
      if anc.cwd and anc.cwd ~= "" then
        return anc.cwd
      end
    end
    if proc.home and proc.home ~= "" then
      return proc.home
    end
  end
  return nil
end

local function find_source_block(win, preferred_block_id)
  if not win or not win.layers then
    return preferred_block_id
  end
  local fallback = nil
  for _, layer in ipairs(win.layers) do
    for _, blk in ipairs(layer.blocks or {}) do
      local lbl = blk.label or ""
      if not OVERLAY_LABELS[lbl] then
        if preferred_block_id and blk.block_id == preferred_block_id then
          return blk.block_id
        end
        if win.focused_block_id and blk.block_id == win.focused_block_id then
          fallback = blk.block_id
        elseif not fallback then
          fallback = blk.block_id
        end
      end
    end
  end
  return fallback or preferred_block_id
end

local function build_hud_command(mode, session_id, cwd)
  local cmd = { resolve_hud_binary(), "ui", "--mode", mode }
  if session_id and session_id ~= "" then
    table.insert(cmd, "--session")
    table.insert(cmd, session_id)
  end
  if cwd and cwd ~= "" then
    table.insert(cmd, "--cwd")
    table.insert(cmd, cwd)
  end
  return cmd
end

rex.action{
  name = "hud_toggle",
  title = "Agents Wallboard: Toggle Floating HUD",
  description = "Toggle the Retina Bento Agents, Repo & Server Telemetry Wallboard overlay.",
  category = "Agents",
  keywords = "hud agents dashboard wallboard telemetry repo vitals float",
  run = function(ctx, _)
    local view, err = rex.session.view{}
    if not view then
      error(err or "failed to inspect Rex session", 0)
    end
    local win = resolve_active_window(view)
    if not win then
      error("no active window in current session", 0)
    end

    for _, layer in ipairs(win.layers or {}) do
      if layer.kind ~= "tiled" then
        for _, blk in ipairs(layer.blocks or {}) do
          if blk.label == FLOAT_LABEL then
            local _, close_err = rex.session.close_layer{
              layer_id = layer.layer_id,
            }
            if close_err ~= nil then
              error(close_err, 0)
            end
            return { closed = true, layer_id = layer.layer_id }
          end
        end
      end
    end

    local src_block = find_source_block(win, ctx.block_id)
    local cwd = resolve_block_cwd(src_block)
    local opts = {
      shell = "none",
      command = build_hud_command("float", view.session_id, cwd),
    }
    if cwd and cwd ~= "" then
      opts.cwd = cwd
    end

    local created, create_err = rex.session.new_layer{
      window_id = win.window_id,
      bounds = { 0.04, 0.05, 0.92, 0.90 },
      focus = true,
      layout = {
        block = {
          flavor = SHELL_FLAVOR,
          label = FLOAT_LABEL,
          options = opts,
        },
      },
    }
    if create_err ~= nil then
      error(create_err, 0)
    end
    local new_block_id = created.block_ids and created.block_ids[1] or nil
    if new_block_id then
      rex.session.set_block_label{
        block_id = new_block_id,
        label = FLOAT_LABEL,
      }
    end
    return {
      opened = true,
      mode = "float",
      layer_id = created.layer_id,
      block_id = new_block_id,
    }
  end,
}

rex.action{
  name = "hud_tab",
  title = "Agents Wallboard: Open or Focus HUD Tab",
  description = "Open or focus a dedicated Agents Wallboard tab with live OSC 7501 status badge.",
  category = "Agents",
  keywords = "hud agents dashboard wallboard tab status",
  run = function(ctx, _)
    local view, err = rex.session.view{}
    if not view then
      error(err or "failed to inspect Rex session", 0)
    end

    for _, w in ipairs(view.windows or {}) do
      for _, layer in ipairs(w.layers or {}) do
        for _, blk in ipairs(layer.blocks or {}) do
          if blk.label == TAB_LABEL then
            rex.session.focus_window{ window_id = w.window_id }
            return { focused = true, window_id = w.window_id, block_id = blk.block_id }
          end
        end
      end
    end

    local active_win = resolve_active_window(view)
    local src_block = find_source_block(active_win, ctx.block_id)
    local cwd = resolve_block_cwd(src_block)
    local opts = {
      shell = "none",
      command = build_hud_command("tab", view.session_id, cwd),
    }
    if cwd and cwd ~= "" then
      opts.cwd = cwd
    end

    local created, create_err = rex.session.new_window{
      window_label = "HUD",
      focus = true,
      layout = {
        block = {
          flavor = SHELL_FLAVOR,
          label = TAB_LABEL,
          options = opts,
        },
      },
    }
    if create_err ~= nil then
      error(create_err, 0)
    end
    local new_block_id = created.block_ids and created.block_ids[1] or nil
    if new_block_id then
      rex.session.set_block_label{
        block_id = new_block_id,
        label = TAB_LABEL,
      }
    end
    return {
      opened = true,
      mode = "tab",
      window_id = created.window_id,
      block_id = new_block_id,
    }
  end,
}

rex.action{
  name = "hud_dock",
  title = "Agents Wallboard: Toggle Docked Side Split",
  description = "Toggle a right-hand docked Agents Wallboard split.",
  category = "Agents",
  keywords = "hud agents dashboard wallboard dock split",
  args = {
    ratio = "number?",
  },
  run = function(ctx, args)
    local view, err = rex.session.view{}
    if not view then
      error(err or "failed to inspect Rex session", 0)
    end
    local win = resolve_active_window(view)
    if not win then
      error("no active window in current session", 0)
    end

    for _, layer in ipairs(win.layers or {}) do
      if layer.kind == "tiled" then
        for _, blk in ipairs(layer.blocks or {}) do
          if blk.label == DOCK_LABEL then
            local _, close_err = rex.call("block.close", {
              session_id = view.session_id,
              block_id = blk.block_id,
            })
            if close_err ~= nil then
              error(close_err, 0)
            end
            return { closed = true, block_id = blk.block_id }
          end
        end
      end
    end

    local src_block = find_source_block(win, ctx.block_id)
    local cwd = resolve_block_cwd(src_block)
    local opts = {
      shell = "none",
      command = build_hud_command("dock", view.session_id, cwd),
    }
    if cwd and cwd ~= "" then
      opts.cwd = cwd
    end

    local anchor_id = src_block
    for _, layer in ipairs(win.layers or {}) do
      if layer.kind == "tiled" then
        if layer.layout and layer.layout.split and layer.layout.split.split_id then
          anchor_id = layer.layout.split.split_id
        elseif not anchor_id and layer.blocks and layer.blocks[1] then
          anchor_id = layer.blocks[1].block_id
        end
        break
      end
    end

    local split_ratio = (args and args.ratio) or 0.42
    local created, split_err = rex.session.new_split{
      anchor_id = anchor_id,
      direction = "horizontal",
      side = "after",
      ratio = split_ratio,
      focus = false,
      layout = {
        block = {
          flavor = SHELL_FLAVOR,
          label = DOCK_LABEL,
          options = opts,
        },
      },
    }
    if split_err ~= nil then
      error(split_err, 0)
    end
    local new_block_id = created.block_ids and created.block_ids[1] or nil
    if new_block_id then
      rex.session.set_block_label{
        block_id = new_block_id,
        label = DOCK_LABEL,
      }
    end
    return {
      opened = true,
      mode = "dock",
      block_id = new_block_id,
    }
  end,
}

local function register_default_binding(chord, action_name)
  pcall(function()
    local existing = rex.bindings{ key = chord }
    if not existing or #existing == 0 then
      rex.bind(chord, action_name)
    end
  end)
end

if IS_WINDOWS then
  register_default_binding("ctrl+shift+u", "hud_toggle")
  register_default_binding("ctrl+alt+u", "hud_dock")
else
  register_default_binding("shift+cmd+u", "hud_toggle")
  register_default_binding("alt+cmd+u", "hud_dock")
end
