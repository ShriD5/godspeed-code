import UIKit

/// What a new session will be created with.
struct NewSessionDraft: Equatable, Codable {
    var projectId: String?
    /// Projectless sessions run on an explicit host.
    var hostId: String?
    var branch: String?
    var worktree = false
    var harness = "claude-code"
    var model: String?
    var effort: String?
}

/// Options the pickers offer (from the core's workspace + host catalogs).
struct ProjectOption: Equatable {
    let id: String
    let name: String
    let path: String
    let device: String
    /// Display name of `device` (never show the id).
    let deviceName: String
    let online: Bool
    let git: Bool
    /// Shared by every checkout of one repository, on any device; with
    /// `groupName` and `colorIndex` they read as one project.
    let groupKey: String
    let groupName: String
    let colorIndex: Int
}

struct HostOption: Equatable {
    let id: String
    let name: String
    let online: Bool
}

struct ModelChoice: Equatable {
    let harness: String
    let harnessLabel: String
    let id: String
    let label: String
    let efforts: [String]
}

/// "Ask anything" → a composer-first canvas. Context is set with native menus
/// (project, branch/worktree, model, effort) that load lazily; the composer is
/// focused immediately so the common case is: tap, type, send.
final class NewSessionViewController: UIViewController, UIGestureRecognizerDelegate {
    private let app: AppModel
    private let onCreated: (String, DraftHandoff?) -> Void
    private let composer = ComposerBar()
    private let hero = UILabel()
    private let wallpaper = WallpaperView()
    private let mark = UIImageView()
    private var draft: NewSessionDraft
    private var models: [ModelChoice] = []

    /// Embedded in the iPad split's main column (no sheet chrome).
    private let embedded: Bool
    private var composerRest: NSLayoutConstraint?
    private var backgroundObserver: NSObjectProtocol?

    deinit {
        if let backgroundObserver { NotificationCenter.default.removeObserver(backgroundObserver) }
    }
    /// Take the keyboard on appearing. The sheet always does; the iPad column
    /// only when opened on purpose (not the launch page).
    var focusOnAppear: Bool

    init(app: AppModel, prompt: String?, embedded: Bool = false, onCreated: @escaping (String, DraftHandoff?) -> Void) {
        self.app = app
        self.embedded = embedded
        self.focusOnAppear = !embedded
        self.onCreated = onCreated
        self.draft = app.lastDraft
        super.init(nibName: nil, bundle: nil)
        // Pick up where the page was left (closed without sending).
        composer.text = prompt ?? app.newSessionText
        composer.images = app.newSessionImages
        // The call owns the microphone: the composer's dictation waits.
        composer.voiceCallActive = app.voice.live
        voiceToken = app.voice.observe { [weak self] in
            guard let self else { return }
            self.composer.voiceCallActive = self.app.voice.live
        }
    }

    private var voiceToken: AnyObject?

    /// A session was created from this page: nothing to keep.
    private var created = false

    /// Closed (swipe down, ✕, or replaced in the iPad column) without
    /// sending: keep what was typed and picked for next time.
    private func rememberDraft() {
        guard !created else { return }
        app.lastDraft = draft
        app.newSessionText = composer.text
        app.newSessionImages = composer.images
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        rememberDraft()
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        // The app can be killed in the background: keep the draft first.
        backgroundObserver = NotificationCenter.default.addObserver(forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main) { [weak self] _ in
            self?.rememberDraft()
        }
        // Tapping the canvas puts the keyboard away (the card stays open here).
        let dismissTap = UITapGestureRecognizer(target: self, action: #selector(dismissKeyboard))
        dismissTap.cancelsTouchesInView = false
        dismissTap.delegate = self
        view.addGestureRecognizer(dismissTap)
        view.backgroundColor = Palette.background
        title = "New Session"
        if !embedded {
            navigationItem.leftBarButtonItem = UIBarButtonItem(systemItem: .close, primaryAction: UIAction { [weak self] _ in
                self?.dismiss(animated: true)
            })
        }

        mark.image = BrandMarks.image(for: draft.harness, side: 34)
        mark.tintColor = Palette.text
        mark.contentMode = .center
        hero.text = "What are we building?"
        hero.numberOfLines = 2
        hero.font = Fonts.ui(.sansSemibold, 22)
        hero.textColor = Palette.text
        hero.textAlignment = .center
        let heroStack = UIStackView(arrangedSubviews: [mark, hero])
        heroStack.axis = .vertical
        heroStack.spacing = 14
        heroStack.alignment = .center
        heroStack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(wallpaper)
        view.addSubview(heroStack)

        composer.translatesAutoresizingMaskIntoConstraints = false
        composer.placeholder = "Describe the task"
        composer.chipsAlwaysVisible = true
        composer.attachMenu = { [weak self] in
            guard let self else { return UIMenu() }
            return AttachmentPicker.menu(host: self, limit: 8 - self.composer.images.count) { [weak self] in self?.composer.addImages($0) }
        }
        composer.onChipTap = { [weak self] id, source in
            guard id == "project" else { return }
            self?.presentProjectPicker(from: source)
        }
        composer.mentionSearch = { [weak self] q in
            guard let self, let p = self.project else { return [] }
            return await self.app.searchFiles(deviceId: p.device, spaceId: p.id, query: q)
        }
        composer.onSend = { [weak self] text, images, _ in
            self?.create(text: text, images: images) ?? false ? .sent : .kept
        }
        view.addSubview(composer)
        heroStack.leadingAnchor.constraint(greaterThanOrEqualTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 24).isActive = true
        // The headline lives in the free space above the composer — from the
        // top of the page to the composer's top edge — centered there, so
        // it's always above the composer (keyboard up or down, one line or
        // two). On iPad the composer keeps the transcript's reading width.
        let free = UILayoutGuide()
        view.addLayoutGuide(free)
        heroStack.setContentCompressionResistancePriority(.required, for: .vertical)
        let fill = composer.widthAnchor.constraint(equalTo: view.safeAreaLayoutGuide.widthAnchor, constant: -24)
        fill.priority = .defaultHigh
        NSLayoutConstraint.activate([
            free.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            free.bottomAnchor.constraint(equalTo: composer.topAnchor, constant: -20),
            free.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            free.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            // Safe area, not view: insets can differ side to side (the
            // iPhone Duo's camera and status bar sit on one edge).
            heroStack.centerXAnchor.constraint(equalTo: view.safeAreaLayoutGuide.centerXAnchor),
            heroStack.centerYAnchor.constraint(equalTo: free.centerYAnchor),
            heroStack.topAnchor.constraint(greaterThanOrEqualTo: free.topAnchor),
            heroStack.bottomAnchor.constraint(lessThanOrEqualTo: free.bottomAnchor),
            composer.centerXAnchor.constraint(equalTo: view.safeAreaLayoutGuide.centerXAnchor),
            composer.leadingAnchor.constraint(greaterThanOrEqualTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 12),
            composer.widthAnchor.constraint(lessThanOrEqualToConstant: 768),
            fill,
        ])
        composerRest = pinAboveKeyboard(composer)
        if draft.projectId == nil, draft.hostId == nil { draft.projectId = app.projectOptions.first?.id }
        if draft.projectId == nil, draft.hostId == nil {
            // No projects yet: run on the first reachable host.
            draft.hostId = app.hostOptions.first(where: \.online)?.id ?? app.hostOptions.first?.id
        }
        loadModels()
    }

    /// The last catalog each host reported, so the chip opens on a real model
    /// name while a fresh one comes back over the relay.
    private static var modelCache: [String: [ModelChoice]] = [:]
    private var modelsDevice: String?

    /// Models for the draft's host: cached (or the built-in catalog) at once,
    /// then the host's own list.
    private func loadModels() {
        let device = deviceId
        guard device != modelsDevice else { return refreshChips() }
        modelsDevice = device
        models = Self.modelCache[device] ?? Self.catalogModels()
        refreshChips()
        Task { [weak self] in
            guard let self else { return }
            let fresh = await self.app.models(for: device)
            guard !fresh.isEmpty else { return }
            Self.modelCache[device] = fresh
            guard self.modelsDevice == device else { return }
            self.models = fresh
            self.refreshChips()
        }
    }

    private static func catalogModels() -> [ModelChoice] {
        fallbackHarnesses().filter(\.offered).flatMap { h in
            fallbackModels(harness: h.id).map { ModelChoice(harness: h.id, harnessLabel: h.label, id: $0.id, label: $0.label, efforts: $0.reasoningLevels) }
        }
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        if focusOnAppear { composer.becomeFirstResponder() }
    }

    override func viewWillLayoutSubviews() {
        super.viewWillLayoutSubviews()
        composerRest?.constant = -composerRestGap
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        // Desktop hero: from the top, 72% of the viewport (≤ 760pt), the
        // artwork softened behind the composer.
        wallpaper.frame = CGRect(x: 0, y: 0, width: view.bounds.width, height: min(view.bounds.height * 0.72, 760))
        wallpaper.cutout = composer.convert(composer.bounds, to: wallpaper)
    }

    @objc private func dismissKeyboard() {
        view.endEditing(true)
    }

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        touch.view.map { !$0.isDescendant(of: composer) } ?? true
    }

    private var project: ProjectOption? { app.projectOptions.first { $0.id == draft.projectId } }
    private var deviceId: String { project?.device ?? draft.hostId ?? "" }

    /// Checkouts of the picked project, one per row of the host menu.
    private var checkouts: [ProjectOption] {
        guard let p = project else { return [] }
        return app.projectOptions
            .filter { $0.groupKey == p.groupKey }
            .sorted { ($0.deviceName.lowercased(), $0.path) < ($1.deviceName.lowercased(), $1.path) }
    }

    private func refreshChips() {
        var chips: [ComposerChip] = []
        if let p = project {
            // The project, then the host it runs on.
            chips.append(ComposerChip(id: "project", title: p.groupName, symbol: nil, icon: ProjectTile.image(name: p.groupName, colorIndex: p.colorIndex)))
            chips.append(ComposerChip(id: "host", title: p.deviceName, symbol: "desktopcomputer"))
            if p.git {
                chips.append(ComposerChip(id: "branch", title: draft.worktree ? "New worktree" : (draft.branch ?? "Current branch"), symbol: nil, icon: BranchIcon.sized()))
            }
        } else {
            let host = app.hostOptions.first { $0.id == draft.hostId }
            chips.append(ComposerChip(id: "project", title: "No project", symbol: "tray"))
            chips.append(ComposerChip(id: "host", title: host?.name ?? "Choose host", symbol: "desktopcomputer"))
        }
        let model = models.first { $0.harness == draft.harness && $0.id == draft.model } ?? models.first { $0.harness == draft.harness }
        // Never the harness name in place of a model: a model this host
        // hasn't listed still gets its catalog label.
        let modelTitle = model?.label ?? draft.model.map { modelLabel(harness: draft.harness, model: $0) } ?? fallbackModels(harness: draft.harness).first?.label ?? HarnessNames.label(draft.harness)
        chips.append(ComposerChip(id: "model", title: modelTitle, symbol: nil, icon: BrandMarks.image(for: draft.harness, side: 13)))
        if let efforts = model?.efforts, !efforts.isEmpty {
            chips.append(ComposerChip(id: "effort", title: (draft.effort ?? efforts[efforts.count / 2]).capitalized, symbol: "gauge.with.dots.needle.67percent"))
        }
        composer.chips = chips
        // The project chip uses a custom picker (search, keyboard, recents)
        // instead of a native UIMenu; taps are handled by `onChipTap`.
        composer.chipMenus = [
            "host": { [weak self] in self?.hostMenu() },
            "branch": { [weak self] in self?.branchMenu() },
            "model": { [weak self] in self?.modelMenu() },
            "effort": { [weak self] in self?.effortMenu() },
        ]
        mark.image = BrandMarks.image(for: draft.harness, side: 34)
    }

    /// One entry per project — checkouts of one repository, on any device,
    /// are one entry listing their hosts; the host chip then picks which.
    private func projectMenu() -> UIMenu {
        var groups: [[ProjectOption]] = []
        for p in app.projectOptions {
            if let i = groups.firstIndex(where: { $0[0].groupKey == p.groupKey }) {
                groups[i].append(p)
            } else {
                groups.append([p])
            }
        }
        groups.sort { $0[0].groupName.localizedCaseInsensitiveCompare($1[0].groupName) == .orderedAscending }
        let picked = project?.groupKey
        var sections: [UIMenuElement] = [UIMenu(options: .displayInline, children: groups.map { members in
            let first = members[0]
            var hosts: [String] = []
            for m in members where !hosts.contains(m.deviceName) { hosts.append(m.deviceName) }
            return UIAction(title: first.groupName, subtitle: hosts.sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending }.joined(separator: ", "), image: ProjectTile.image(name: first.groupName, colorIndex: first.colorIndex, side: 18), state: first.groupKey == picked ? .on : .off) { [weak self] _ in
                self?.pickProject(members)
            }
        })]
        sections.append(UIMenu(options: .displayInline, children: [
            UIAction(title: "No Project…", image: UIImage(systemName: "tray"), state: draft.projectId == nil ? .on : .off) { [weak self] _ in
                guard let self else { return }
                self.draft.projectId = nil
                self.draft.hostId = self.draft.hostId ?? self.app.hostOptions.first(where: \.online)?.id ?? self.app.hostOptions.first?.id
                self.loadModels()
            },
            UIAction(title: "New Project…", image: UIImage(systemName: "folder.badge.plus")) { [weak self] _ in
                guard let self else { return }
                let vc = NewProjectViewController(app: self.app)
                vc.onCreated = { [weak self] id in
                    self?.draft.projectId = id
                    self?.draft.hostId = nil
                    self?.draft.branch = nil
                    self?.loadModels()
                }
                self.present(UINavigationController(rootViewController: vc), animated: true)
            },
        ]))
        return UIMenu(title: "Project", children: sections)
    }

    /// Keep the host when it has a checkout of the project, else the first
    /// reachable one.
    private func pickProject(_ members: [ProjectOption]) {
        let pick = members.first { $0.id == draft.projectId }
            ?? members.first { $0.device == deviceId }
            ?? members.first(where: \.online)
            ?? members[0]
        pickCheckout(pick)
    }

    private func pickCheckout(_ p: ProjectOption) {
        if p.id != draft.projectId { draft.branch = nil }
        draft.projectId = p.id
        draft.hostId = nil
        loadModels()
    }

    /// With a project: its checkouts, by host (the path tells apart several
    /// on one host). Without: every host — project-less sessions run in its
    /// home.
    private func hostMenu() -> UIMenu {
        let checkouts = self.checkouts
        if !checkouts.isEmpty {
            return UIMenu(title: "Run on", children: checkouts.map { p in
                let shared = checkouts.filter { $0.device == p.device }.count > 1
                return UIAction(title: p.deviceName, subtitle: shared ? p.path : (p.online ? "Online" : "Offline"), image: UIImage(systemName: "desktopcomputer"), state: p.id == draft.projectId ? .on : .off) { [weak self] _ in
                    self?.pickCheckout(p)
                }
            })
        }
        return UIMenu(title: "Run on", children: app.hostOptions.map { h in
            UIAction(title: h.name, subtitle: h.online ? "Online" : "Offline", image: UIImage(systemName: "desktopcomputer"), state: h.id == draft.hostId ? .on : .off) { [weak self] _ in
                self?.draft.hostId = h.id
                self?.loadModels()
            }
        })
    }

    private func branchMenu() -> UIMenu {
        UIMenu(title: "Checkout", children: [
            UIAction(title: "New worktree", state: draft.worktree ? .on : .off) { [weak self] _ in
                self?.draft.worktree.toggle()
                self?.refreshChips()
            },
            UIMenu(title: "Branch", options: .displayInline, children: [UIDeferredMenuElement { [weak self] done in
                guard let self, let p = self.project else { return done([]) }
                Task { @MainActor in
                    let refs = await self.app.refs(projectId: p.id)
                    done(refs.map { ref in
                        UIAction(title: ref, state: ref == (self.draft.branch ?? refs.first) ? .on : .off) { [weak self] _ in
                            self?.draft.branch = ref
                            self?.refreshChips()
                        }
                    })
                }
            }]),
        ])
    }

    private func modelMenu() -> UIMenu {
        let byHarness = Dictionary(grouping: models, by: \.harness)
        return UIMenu(title: "Model", children: byHarness.keys.sorted().map { h in
            UIMenu(title: byHarness[h]!.first!.harnessLabel, image: BrandMarks.image(for: h, side: 16), options: .displayInline, children: byHarness[h]!.map { m in
                UIAction(title: m.label, state: m.harness == draft.harness && m.id == draft.model ? .on : .off) { [weak self] _ in
                    self?.draft.harness = m.harness
                    self?.draft.model = m.id
                    self?.draft.effort = nil
                    self?.refreshChips()
                }
            })
        })
    }

    private func effortMenu() -> UIMenu {
        let model = models.first { $0.harness == draft.harness && $0.id == draft.model } ?? models.first { $0.harness == draft.harness }
        return UIMenu(title: "Reasoning effort", children: (model?.efforts ?? []).map { e in
            UIAction(title: e.capitalized, state: e == draft.effort ? .on : .off) { [weak self] _ in
                self?.draft.effort = e
                self?.refreshChips()
            }
        })
    }

    /// False when the session couldn't be created (the prompt stays put).
    private func create(text: String, images: [StagedImage]) -> Bool {
        app.lastDraft = draft
        guard let chatId = app.createSession(draft: draft, text: text, images: images) else {
            let alert = UIAlertController(title: "Couldn't start the session", message: "Choose a project or a host that can run it.", preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "OK", style: .default))
            present(alert, animated: true)
            return false
        }
        created = true
        PushNotifications.shared.askAfterFirstSession()
        app.newSessionText = ""
        app.newSessionImages = []
        // Lift the draft out (page + composer + typed text) so the chat can
        // take over in one motion.
        onCreated(chatId, DraftHandoff.capture(from: self, composer: composer, text: text))
        return true
    }
}

enum HarnessNames {
    /// The core's harness catalog label (same table as desktop).
    static func label(_ id: String) -> String { harnessLabel(harness: id) }
}

// MARK: - Custom project picker (search + keyboard + recents)

private struct ProjectGroupChoice {
    let key: String
    let name: String
    let members: [ProjectOption]
    let colorIndex: Int
    var hostsSubtitle: String {
        var hosts: [String] = []
        for m in members where !hosts.contains(m.deviceName) { hosts.append(m.deviceName) }
        return hosts.sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending }.joined(separator: ", ")
    }
}

private extension NewSessionViewController {
    func presentProjectPicker(from source: UIView) {
        let groups = projectGroups()
        let recentKeys = recentProjectGroupKeys()
        let picker = ProjectPickerSheet(
            groups: groups,
            recentGroupKeys: recentKeys,
            selectedGroupKey: project?.groupKey,
            selectedNoProject: draft.projectId == nil
        )
        picker.onPickGroup = { [weak self] members in
            self?.dismiss(animated: true)
            self?.pickProject(members)
        }
        picker.onPickNoProject = { [weak self] in
            self?.dismiss(animated: true)
            guard let self else { return }
            self.draft.projectId = nil
            self.draft.hostId = self.draft.hostId ?? self.app.hostOptions.first(where: \.online)?.id ?? self.app.hostOptions.first?.id
            self.loadModels()
        }
        picker.onNewProject = { [weak self] in
            self?.dismiss(animated: true)
            guard let self else { return }
            let vc = NewProjectViewController(app: self.app)
            vc.onCreated = { [weak self] id in
                self?.draft.projectId = id
                self?.draft.hostId = nil
                self?.draft.branch = nil
                self?.loadModels()
            }
            self.present(UINavigationController(rootViewController: vc), animated: true)
        }
        let nav = UINavigationController(rootViewController: picker)
        nav.modalPresentationStyle = .popover
        if let pop = nav.popoverPresentationController {
            pop.sourceView = source
            pop.sourceRect = source.bounds
            pop.permittedArrowDirections = [.up, .down]
        } else {
            if let sheet = nav.sheetPresentationController {
                sheet.detents = [.medium(), .large()]
                sheet.prefersGrabberVisible = true
            }
        }
        present(nav, animated: true)
    }

    func projectGroups() -> [ProjectGroupChoice] {
        var groups: [[ProjectOption]] = []
        for p in app.projectOptions {
            if let i = groups.firstIndex(where: { $0[0].groupKey == p.groupKey }) {
                groups[i].append(p)
            } else {
                groups.append([p])
            }
        }
        var choices = groups.map { members in
            let first = members[0]
            return ProjectGroupChoice(key: first.groupKey, name: first.groupName, members: members, colorIndex: first.colorIndex)
        }
        choices.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        return choices
    }

    func recentProjectGroupKeys() -> [String] {
        // `frontPage.projectGroups` are in recency order; ids are "project:<key>".
        var keys: [String] = []
        for g in app.frontPage.projectGroups {
            if g.id.hasPrefix("project:") {
                let key = String(g.id.dropFirst("project:".count))
                if !keys.contains(key) { keys.append(key) }
            }
            if keys.count >= 5 { break }
        }
        return keys
    }
}

private final class ProjectPickerSheet: UITableViewController, UISearchResultsUpdating {
    private let allGroups: [ProjectGroupChoice]
    private let recentGroupKeys: [String]
    private let selectedGroupKey: String?
    private let selectedNoProject: bool
    private var filtered: [ProjectGroupChoice] = []
    private var highlight = 0
    private let searchController = UISearchController(searchResultsController: nil)
    private var recentChipButtons: [UIButton] = []

    var onPickGroup: (([ProjectOption]) -> Void)?
    var onPickNoProject: (() -> Void)?
    var onNewProject: (() -> Void)?

    init(groups: [ProjectGroupChoice], recentGroupKeys: [String], selectedGroupKey: String?, selectedNoProject: Bool) {
        self.allGroups = groups
        self.recentGroupKeys = recentGroupKeys
        self.selectedGroupKey = selectedGroupKey
        self.selectedNoProject = selectedNoProject
        super.init(style: .insetGrouped)
        self.filtered = groups
    }

    required init?(coder: NSCoder) { fatalError() }

    override var canBecomeFirstResponder: Bool { true }

    override var keyCommands: [UIKeyCommand]? {
        let up = UIKeyCommand(input: UIKeyCommand.inputUpArrow, modifierFlags: [], action: #selector(moveUp))
        let down = UIKeyCommand(input: UIKeyCommand.inputDownArrow, modifierFlags: [], action: #selector(moveDown))
        let enter = UIKeyCommand(input: "\r", modifierFlags: [], action: #selector(activateHighlight))
        up.wantsPriorityOverSystemBehavior = true
        down.wantsPriorityOverSystemBehavior = true
        var cmds = [up, down, enter]
        let q = (searchController.searchBar.text ?? "")
        if q.isEmpty {
            for n in 1...5 {
                let cmd = UIKeyCommand(input: "\(n)", modifierFlags: [], action: #selector(numberKey(_:)))
                cmds.append(cmd)
            }
        }
        return cmds
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Project"
        navigationItem.leftBarButtonItem = UIBarButtonItem(systemItem: .close, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        searchController.searchResultsUpdater = self
        searchController.obscuresBackgroundDuringPresentation = false
        searchController.searchBar.placeholder = "Search projects"
        navigationItem.searchController = searchController
        navigationItem.hidesSearchBarWhenScrolling = false
        definesPresentationContext = true
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: "cell")
        tableView.keyboardDismissMode = .onDrag
        rebuildRecentsHeader()
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        becomeFirstResponder()
    }

    private func rebuildRecentsHeader() {
        let q = (searchController.searchBar.text ?? "")
        let recents = recentGroupKeys.prefix(4).compactMap { key in allGroups.first { $0.key == key } }
        guard q.isEmpty, !recents.isEmpty else {
            tableView.tableHeaderView = nil
            recentChipButtons = []
            return
        }
        let wrap = UIView()
        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 8
        stack.alignment = .center
        stack.translatesAutoresizingMaskIntoConstraints = false
        recentChipButtons = []
        for (i, g) in recents.enumerated() {
            let n = i + 1
            let b = UIButton(type: .system)
            var config = UIButton.Configuration.tinted()
            config.title = "\(n)  \(g.name)"
            config.cornerStyle = .capsule
            config.baseForegroundColor = .label
            config.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 10, bottom: 6, trailing: 10)
            b.configuration = config
            b.tag = n
            b.addAction(UIAction { [weak self] _ in self?.pickRecent(n) }, for: .touchUpInside)
            stack.addArrangedSubview(b)
            recentChipButtons.append(b)
        }
        wrap.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: wrap.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(lessThanOrEqualTo: wrap.trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: wrap.topAnchor, constant: 8),
            stack.bottomAnchor.constraint(equalTo: wrap.bottomAnchor, constant: -8),
        ])
        wrap.frame = CGRect(x: 0, y: 0, width: tableView.bounds.width, height: 52)
        tableView.tableHeaderView = wrap
    }

    private var rowItems: [RowItem] {
        var items = filtered.map { RowItem.group($0) }
        let q = (searchController.searchBar.text ?? "")
        if q.isEmpty {
            items.append(.newProject)
            items.append(.noProject)
        }
        return items
    }

    private enum RowItem {
        case group(ProjectGroupChoice)
        case newProject
        case noProject
    }

    private func pickRecent(_ n: Int) {
        let recents = recentGroupKeys.prefix(4).compactMap { key in allGroups.first { $0.key == key } }
        guard n >= 1, n <= recents.count else { return }
        onPickGroup?(recents[n - 1].members)
    }

    @objc private func numberKey(_ cmd: UIKeyCommand) {
        guard let t = cmd.input, let n = Int(t) else { return }
        pickRecent(n)
    }

    @objc private func moveUp() {
        let count = max(1, rowItems.count)
        highlight = max(0, highlight - 1)
        tableView.reloadData()
        if highlight < count {
            tableView.scrollToRow(at: IndexPath(row: highlight, section: 0), at: .none, animated: false)
        }
    }

    @objc private func moveDown() {
        let count = rowItems.count
        guard count > 0 else { return }
        highlight = min(count - 1, highlight + 1)
        tableView.reloadData()
        tableView.scrollToRow(at: IndexPath(row: highlight, section: 0), at: .none, animated: false)
    }

    @objc private func activateHighlight() {
        let items = rowItems
        guard highlight >= 0, highlight < items.count else { return }
        select(items[highlight])
    }

    private func select(_ item: RowItem) {
        switch item {
        case .group(let g): onPickGroup?(g.members)
        case .newProject: onNewProject?()
        case .noProject: onPickNoProject?()
        }
    }

    func updateSearchResults(for searchController: UISearchController) {
        let q = (searchController.searchBar.text ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if q.isEmpty {
            filtered = allGroups
        } else {
            filtered = allGroups.filter {
                $0.name.localizedCaseInsensitiveContains(q) || $0.hostsSubtitle.localizedCaseInsensitiveContains(q)
            }
        }
        highlight = 0
        rebuildRecentsHeader()
        tableView.reloadData()
    }

    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        rowItems.count
    }

    override func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let cell = tableView.dequeueReusableCell(withIdentifier: "cell", for: indexPath)
        var config = cell.defaultContentConfiguration()
        let item = rowItems[indexPath.row]
        switch item {
        case .group(let g):
            config.text = g.name
            config.secondaryText = g.hostsSubtitle
            config.image = ProjectTile.image(name: g.name, colorIndex: g.colorIndex, side: 20)
            cell.accessoryType = (g.key == selectedGroupKey) ? .checkmark : .none
        case .newProject:
            config.text = "New Project…"
            config.image = UIImage(systemName: "folder.badge.plus")
            cell.accessoryType = .none
        case .noProject:
            config.text = "No Project…"
            config.image = UIImage(systemName: "tray")
            cell.accessoryType = selectedNoProject ? .checkmark : .none
        }
        cell.contentConfiguration = config
        if indexPath.row == highlight {
            cell.backgroundColor = UIColor.tintColor.withAlphaComponent(0.08)
        } else {
            cell.backgroundColor = nil
        }
        return cell
    }

    override func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
        tableView.deselectRow(at: indexPath, animated: true)
        highlight = indexPath.row
        select(rowItems[indexPath.row])
    }
}
