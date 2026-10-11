-- Built-in /rewind command. Reads bounded pages from the transcript index.

smelt.cmd.register("rewind", function(args)
  local restore_vim_insert = (args or ""):find("insert", 1, true) ~= nil or (smelt.vim.mode() == "insert")
  local quiet = (args or ""):find("quiet", 1, true) ~= nil
  local session = smelt.session.id()
  local epoch = smelt.signal.get("history_epoch")
  local guard = smelt.work.guard()

  local function current()
    return session == smelt.session.id()
      and epoch == smelt.signal.get("history_epoch")
      and smelt.work.guard_current(guard)
  end

  smelt.spawn(function()
    local cursors = {}
    local before = nil
    while current() do
      local busy = smelt.work.busy("loading rewind turns")
      local ok, page = pcall(smelt.session.turns_page, before)
      busy:remove()
      if not current() then return end
      if not ok then smelt.notify.error(tostring(page)); return end
      local turns = page.turns
      if #turns == 0 then
        if not quiet then smelt.notify.error("nothing to rewind") end
        return
      end

      local items, actions = {}, {}
      local function add(label, action)
        items[#items + 1] = label
        actions[#actions + 1] = action
      end
      if page.next_before then add("(older turns)", "older") end
      for _, turn in ipairs(turns) do add(turn.label or "", turn.history_idx) end
      if #cursors > 0 then add("(newer turns)", "newer") end
      add("(current)", "current")

      local leaf = smelt.dialog.menu(items, {
        selected = #items,
        wrap = true,
        -- Digits only move the cursor. Enter confirms a destructive rewind.
        shortcuts = "select",
      })
      local picked = smelt.dialog.open({
        title = "rewind", max_height = "100%", min_height = 0,
        panels = { { leaf = leaf } },
      })
      if not picked or not picked.index or not current() then return end
      local action = actions[picked.index]
      if action == "older" then
        cursors[#cursors + 1] = before or false
        before = page.next_before
      elseif action == "newer" then
        before = table.remove(cursors) or nil
      elseif action == "current" then
        if restore_vim_insert then smelt.vim.set_mode("insert") end
        return
      else
        smelt.session.rewind_to(action, { restore_vim_insert = restore_vim_insert })
        return
      end
    end
  end)
end, { desc = "rewind to a previous turn" })
