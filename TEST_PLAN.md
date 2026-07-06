# NestWeaver UI Overhaul P0 Manual Test Plan

This plan covers P0 foundation and trust repair only. Do not execute these cases until implementation testing begins. Use a current P0 build of the web UI and a representative indexed database with at least one searchable symbol, at least one overview graph, and enough nodes to show labels, edges, selection, and mode changes.

For every case, record the browser, operating system, build or commit under test, theme, motion setting, and whether the UI is running against fixture data or a real local database.

## TC-01 Graph Canvas Is Nonblank, Framed, and Usable

1. Open the NestWeaver web UI.
   - Expected result: The top bar, graph panel, control dock, and graph canvas area appear without layout overlap.
   - Evidence to capture: Full-page screenshot after first paint.
2. Wait until initial graph loading finishes.
   - Expected result: The canvas is not blank; visible nodes and/or labels are inside the graph panel frame, not clipped offscreen or hidden behind controls.
   - Evidence to capture: Screenshot of the graph panel and browser console screenshot if anything looks blank.
3. Select or focus a visible graph node.
   - Expected result: Selection is visually obvious and the node remains in frame.
   - Evidence to capture: Screenshot showing the selected/focused node.

Overall success criteria: A user can visually orient in the graph without seeing a blank canvas, an offscreen scene, unreadable haze, or controls covering the primary graph.

## TC-02 Brand, Palette, and Bloom Readability in Light Theme

1. Set the theme control to Light.
   - Expected result: The app visibly switches to light theme and remains readable.
   - Evidence to capture: Screenshot of the full UI in light theme.
2. Inspect the graph with ordinary, selected, and focused nodes visible.
   - Expected result: Node kind colors remain distinct; labels, edges, selected node marks, and focus marks have enough contrast against the light graph background.
   - Evidence to capture: Close screenshot of the graph with at least two node kinds and one selected node.
3. Open at least one control surface such as the scope control, theme control, or settings/control dock.
   - Expected result: Text, borders, focus rings, and active states match the branded UI and remain legible.
   - Evidence to capture: Screenshot of the opened control.

Overall success criteria: Light theme communicates graph kind, selection, focus, and controls clearly without washed-out text or color-only ambiguity.

## TC-03 Brand, Palette, and Selective Bloom Readability in Dark Theme

1. Set the theme control to Dark.
   - Expected result: The app visibly switches to dark theme and the graph background uses the dark graph surface.
   - Evidence to capture: Full-page screenshot in dark theme.
2. Inspect a dense graph area.
   - Expected result: Bloom is present only as meaningful emphasis on selected, focused, hub, bridge, or important nodes; the dense overview does not become a glowing haze.
   - Evidence to capture: Screenshot of the densest visible graph area.
3. Select a function-colored node or similarly bright node.
   - Expected result: Selection and focus remain visible against the node color and bloom.
   - Evidence to capture: Screenshot of the selected node.

Overall success criteria: Dark theme feels intentionally branded while preserving graph readability, kind distinction, and selected/focused state clarity.

## TC-04 Reduced Motion Preserves Meaning and Removes Nonessential Motion

1. Enable the operating system or browser reduced-motion preference before opening the UI.
   - Expected result: The UI starts with reduced effects enabled without requiring manual toggling.
   - Evidence to capture: Screenshot or screen recording showing the reduced-effects state after load.
2. Open the graph in dark theme with reduced motion active.
   - Expected result: Nonessential travel, ripple, breathing, and motion-heavy bloom behavior is disabled; static node color, size, labels, rings, and selection still communicate meaning.
   - Evidence to capture: Short screen recording of the graph after load.
3. Change the OS or browser reduced-motion setting to reduce while the UI is open, if the environment allows it.
   - Expected result: The UI enables reduced effects when the preference changes.
   - Evidence to capture: Screen recording or screenshots before and after the preference change.

Overall success criteria: Reduced-motion users receive the same graph meaning without nonessential animation or motion-heavy effects.

## TC-05 Cmd+K Query Dialog

1. Focus the main graph panel or top-level app surface.
   - Expected result: Focus is visibly somewhere outside the query dialog.
   - Evidence to capture: Note the starting focus target.
2. Press Cmd+K on macOS or Ctrl+K on non-macOS.
   - Expected result: The query dialog opens, has a clear dialog title, and the query input receives focus.
   - Evidence to capture: Screenshot of the dialog and note the focused field.
3. Type a short query but do not submit it.
   - Expected result: Text entry works and the rest of the page is visually and semantically behind the modal surface.
   - Evidence to capture: Screenshot with typed text.
4. Press Escape.
   - Expected result: The dialog closes and focus returns to the element or surface that had focus before opening.
   - Evidence to capture: Note the returned focus target.

Overall success criteria: Cmd+K reliably opens the query surface from the main UI, keyboard focus is correct, and closing restores user orientation.

## TC-06 Query Dialog Loading, Error, Cancelled, and Aria-Live Feedback

1. Open the query dialog with Cmd+K and submit a valid query that takes noticeable time.
   - Expected result: A visible loading or progress state appears while the query is running.
   - Evidence to capture: Screenshot or screen recording of the loading state.
2. Cancel or close the query while it is still running, if cancellation is supported.
   - Expected result: The UI shows or announces that the operation was cancelled, and no stale result appears after cancellation.
   - Evidence to capture: Screen recording and any visible notification.
3. Submit a query while the query endpoint is forced to fail or while the backend is unavailable.
   - Expected result: A visible error notification or inline error appears, and the failure is announced through the live region.
   - Evidence to capture: Screenshot of the error and assistive technology announcement log if available.
4. Submit a query that returns no usable result.
   - Expected result: Empty or no-match state is distinct from loading, cancelled, and error.
   - Evidence to capture: Screenshot of the empty/no-match state.

Overall success criteria: Query users always know whether the operation is loading, cancelled, empty, or failed, and meaningful state changes are accessible through `aria-live`.

## TC-07 Question Mark Shortcut Overlay

1. Focus the graph panel or another non-text-entry surface.
   - Expected result: The UI is ready for global shortcuts.
   - Evidence to capture: Note the starting focus target.
2. Press `?`.
   - Expected result: The keyboard shortcuts dialog opens with a clear title and useful shortcut groups.
   - Evidence to capture: Screenshot of the overlay.
3. Press Tab repeatedly.
   - Expected result: Focus stays inside the overlay and reaches the close control or other focusable elements.
   - Evidence to capture: Notes or screen recording of focus movement.
4. Press Escape.
   - Expected result: The overlay closes and focus returns to the prior surface.
   - Evidence to capture: Note the returned focus target.

Overall success criteria: The shortcut overlay is discoverable, keyboard accessible, focus-trapped while open, and restores focus on close.

## TC-08 Visible Mode Tabs and Keyboard Reachability

1. Locate the mode tabs in the graph panel.
   - Expected result: Overview, Context, Impact, Repos, Features, and Local modes are visible or otherwise available in the P0 tab set.
   - Evidence to capture: Screenshot of the tabs.
2. Use Tab from the main UI to reach the mode tabs.
   - Expected result: Each mode tab can receive keyboard focus.
   - Evidence to capture: Screen recording of keyboard focus reaching the tabs.
3. Activate each reachable mode with Enter or Space.
   - Expected result: The active mode changes, and selected state is visually and semantically conveyed.
   - Evidence to capture: Screenshot of at least two active mode states.
4. Use numeric shortcuts 1 through 6 from the graph panel, if implemented for P0.
   - Expected result: The graph mode changes to the matching mode without requiring a mouse.
   - Evidence to capture: Notes mapping each key to the active mode observed.

Overall success criteria: Mode switching is visible, keyboard reachable, and does not depend on hidden indicators or mouse-only interaction.

## TC-09 Explicit System, Light, and Dark Theme Control

1. Open the theme control.
   - Expected result: System, Light, and Dark are shown as explicit choices.
   - Evidence to capture: Screenshot of the open theme menu.
2. Choose Light.
   - Expected result: The app switches to light theme and the choice is reflected in the control.
   - Evidence to capture: Full-page screenshot.
3. Choose Dark.
   - Expected result: The app switches to dark theme and the choice is reflected in the control.
   - Evidence to capture: Full-page screenshot.
4. Choose System while the OS/browser is set to a known color scheme.
   - Expected result: The app follows the system preference and keeps graph readability.
   - Evidence to capture: Screenshot plus note of the OS/browser color-scheme setting.

Overall success criteria: Theme selection is explicit, keyboard operable, persistent as designed, and does not make the graph or controls unreadable.

## TC-10 Scope Controls

1. Open the top-bar scope control.
   - Expected result: All, Code only, and Notes only are available and readable.
   - Evidence to capture: Screenshot of the open control.
2. Select each scope option from the keyboard.
   - Expected result: The selected value updates and the graph/search surface remains usable.
   - Evidence to capture: Notes for each selected option.
3. Open the control dock or settings area and locate its scope control.
   - Expected result: The dock scope control exposes the same options and current value.
   - Evidence to capture: Screenshot of the dock scope control.
4. Change scope from the dock.
   - Expected result: The top-bar and dock controls remain consistent, and focus does not get lost.
   - Evidence to capture: Screenshot after the change.

Overall success criteria: Scope controls are branded, explicit, keyboard operable, and consistent across locations.

## TC-11 Loading, Empty, Error, SSE, Indexing, Export, and Admin Feedback

1. Load the UI during normal backend availability.
   - Expected result: Initial loading is visibly distinct from the loaded graph state.
   - Evidence to capture: Screenshot or screen recording of loading.
2. Use fixture data or backend routing to produce an empty overview or empty search result.
   - Expected result: Empty state explains that no content/no results are available and does not look like an error or endless loading.
   - Evidence to capture: Screenshot of the empty state.
3. Force a user-facing API failure for search, status, graph load, gap analysis, compare, export, indexing, or admin action where available.
   - Expected result: The UI shows a visible notification or inline error and announces a meaningful message through the live region.
   - Evidence to capture: Screenshot of the notification and assistive technology announcement log if available.
4. Trigger an export action.
   - Expected result: Success, progress, cancellation, or failure feedback appears as appropriate for the action outcome.
   - Evidence to capture: Screenshot of feedback or downloaded file evidence.
5. Observe an SSE-style graph/status update or indexing status update.
   - Expected result: The update is visible and announced without disrupting active focus or graph orientation.
   - Evidence to capture: Screen recording showing the update.

Overall success criteria: User-facing async states are visible and accessible; empty never masks error, and long-running or backend-backed actions do not fail silently.

## TC-12 Modal and Popover Focus Trap, Keyboard Operation, and Focus Return

1. Open the query dialog, shortcut overlay, theme menu, scope select, control dock popover, export menu, context menu, and perspective popover where available.
   - Expected result: Each surface opens from the keyboard or a clearly reachable control.
   - Evidence to capture: Notes naming each surface tested.
2. For each modal surface, press Tab and Shift+Tab several times.
   - Expected result: Focus remains inside the modal until closed.
   - Evidence to capture: Screen recording for at least the query dialog and shortcut overlay.
3. For each popover/menu surface, navigate options with keyboard controls.
   - Expected result: Options can be reached and activated without mouse hover.
   - Evidence to capture: Notes for each surface.
4. Close each surface with Escape or its close action.
   - Expected result: Focus returns to the opener or a logical nearby control.
   - Evidence to capture: Notes naming the returned focus target.

Overall success criteria: Modal and popover surfaces have usable keyboard semantics, no focus leaks, and predictable focus return.

## TC-13 Graph Orientation Preservation Across Reload or SSE-Style Updates

1. Open a graph scene with several visible nodes and select one node near the center.
   - Expected result: The selected node, camera orientation, and nearby node positions are easy to identify.
   - Evidence to capture: Screenshot before refresh/update.
2. Pan and zoom the graph to a recognizable orientation.
   - Expected result: The camera view changes and remains stable while idle.
   - Evidence to capture: Screenshot after pan/zoom.
3. Trigger a graph reload, re-index notification, or SSE-style graph update that does not change graph identity/topology.
   - Expected result: The camera, selected node, and known node positions remain stable instead of randomizing.
   - Evidence to capture: Before/after screenshots or screen recording.
4. Trigger an update that adds a small number of new nodes, if practical.
   - Expected result: Existing nodes stay in place; new nodes appear near known neighbors or in deterministic positions.
   - Evidence to capture: Before/after screenshots.

Overall success criteria: Graph updates preserve user orientation when identity/topology is unchanged and avoid randomizing active scenes.

## TC-14 Superseded Graph Response Protection

1. Start a context graph request from a search result or selected node.
   - Expected result: The UI enters a loading state for the requested context.
   - Evidence to capture: Note the first requested symbol/query.
2. Before the first request finishes, start a second context request for a different symbol or query.
   - Expected result: The UI reflects the second request as the current intent.
   - Evidence to capture: Note the second requested symbol/query.
3. Let both requests finish in any order.
   - Expected result: The final graph, selected node, labels, and any notifications correspond to the second request; the first response does not overwrite newer state.
   - Evidence to capture: Screen recording or final screenshot.
4. Repeat with Impact mode or another async graph action that P0 guards.
   - Expected result: Older impact/action responses cannot replace the newer selected node or newer mode state.
   - Evidence to capture: Screen recording or screenshots.

Overall success criteria: Fast successive graph requests cannot show stale graph results, stale selected nodes, or stale errors after a newer request wins.

## TC-15 Regression Smoke for Existing Graph Explorer Behavior

1. Open the graph explorer normally.
   - Expected result: Focus Map or the configured default graph experience opens without regressions.
   - Evidence to capture: Screenshot of initial graph explorer state.
2. Use the search input to find a known symbol and open a context scene.
   - Expected result: Search results appear, selecting a result updates the graph/context state, and the app remains responsive.
   - Evidence to capture: Screenshot after selecting the result.
3. Open the control dock/settings area and switch to list view.
   - Expected result: Ranked node table appears and can select a node.
   - Evidence to capture: Screenshot of list view.
4. Switch to matrix view.
   - Expected result: Graph matrix view appears and row selection works.
   - Evidence to capture: Screenshot of matrix view.
5. Export the current graph as PNG, SVG, and HTML where supported.
   - Expected result: Downloads complete and files are nonempty.
   - Evidence to capture: Download filenames and file sizes.

Overall success criteria: Existing graph explorer workflows continue to work after P0 changes.

## TC-16 Keyboard-Only Golden Path

1. Starting from page load, use only the keyboard to focus the search input.
   - Expected result: Search input receives focus.
   - Evidence to capture: Notes or screen recording.
2. Search for a known symbol and select a result using only the keyboard.
   - Expected result: The graph opens or updates to a context scene for the selected symbol.
   - Evidence to capture: Screenshot after selection.
3. Use keyboard navigation to reach the graph panel, select a node, and open available node actions.
   - Expected result: Node selection and actions are available without mouse hover.
   - Evidence to capture: Screen recording.
4. Use keyboard navigation to switch mode tabs and open the shortcut overlay.
   - Expected result: Mode tabs and shortcut overlay are both reachable and operable.
   - Evidence to capture: Screen recording.

Overall success criteria: A keyboard-only user can complete a basic orient/search/explore path and recover help through the shortcut overlay.

## Automated Companion Checks

These are not manual steps, but they should be paired with the manual run before P0 is called done:

1. Visual Playwright check confirms the graph canvas is visible, nonblank, and framed.
   - Expected result: Automated evidence includes a screenshot or pixel check proving a rendered graph area.
2. Keyboard Playwright check covers Cmd+K, the `?` overlay, mode tab reachability, and at least one keyboard-only smoke path.
   - Expected result: Automated run passes without manual mouse input.
3. Existing graph explorer Playwright smoke remains green.
   - Expected result: Existing search, graph/list/matrix, export, empty overview, and graph API smoke coverage still passes.

Overall success criteria: Manual evidence and automated release gates agree that the P0 foundation is usable, accessible, and regression-safe.

## P0 Exit Evidence Checklist

- Full-page screenshots for Light, Dark, and System themes.
- Graph close-ups showing ordinary, selected, focused, hub/bridge/important nodes where available.
- Reduced-motion screenshot or recording.
- Cmd+K query dialog screenshot and focus-return notes.
- Shortcut overlay screenshot and focus-trap notes.
- Mode tabs keyboard recording or notes.
- Scope controls screenshots from top bar and dock.
- Loading, empty, cancelled, error, and success/progress feedback evidence where each state is available.
- Aria-live announcement evidence from assistive technology, accessibility tooling, or reviewer notes.
- Before/after graph orientation screenshots for reload or SSE-style updates.
- Superseded-response recording or notes.
- Existing graph explorer regression evidence.
