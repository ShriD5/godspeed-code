import UIKit

/// Mission Control: every agent on every device at a glance — status pill,
/// how long it has been at it, what it's doing or waiting on, branch and
/// subagents — grouped by device, then project, most urgent first. Live:
/// it follows the registry and the inbox, and its timers tick on screen.
final class BoardViewController: UIViewController, UICollectionViewDelegate {
    private enum Item: Hashable {
        case totals
        case project(device: String, key: String)
        case card(String)
    }

    private let app: AppModel
    private var collectionView: UICollectionView!
    private var dataSource: UICollectionViewDiffableDataSource<String, Item>!
    private var model = BoardModel()
    private var cards: [String: BoardModel.Card] = [:]
    private var token: AnyObject?
    private var clock: Timer?

    init(app: AppModel) {
        self.app = app
        super.init(nibName: nil, bundle: nil)
        title = "Mission Control"
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.largeTitleDisplayMode = .always
        var config = UICollectionLayoutListConfiguration(appearance: .plain)
        config.backgroundColor = .clear
        config.showsSeparators = false
        config.headerMode = .supplementary
        config.headerTopPadding = 6
        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: UICollectionViewCompositionalLayout.list(using: config))
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.delegate = self
        collectionView.accessibilityIdentifier = "board-list"
        view.addSubview(collectionView)

        let totalsReg = UICollectionView.CellRegistration<UICollectionViewListCell, Item> { [weak self] cell, _, _ in
            guard let self else { return }
            cell.contentConfiguration = BoardTotalsConfiguration(totals: self.model.totals)
            var bg = UIBackgroundConfiguration.clear()
            bg.backgroundColor = .clear
            cell.backgroundConfiguration = bg
            cell.accessibilityIdentifier = "board-totals"
        }
        let projectReg = UICollectionView.CellRegistration<UICollectionViewListCell, Item> { [weak self] cell, _, item in
            guard let self, case let .project(device, key) = item,
                  let group = self.model.devices.first(where: { $0.id == device })?.projects.first(where: { $0.key == key }) else { return }
            var c = UIListContentConfiguration.cell()
            c.text = group.name
            c.textProperties.font = Fonts.ui(.sansSemibold, TypeScale.size(13))
            c.textProperties.color = Palette.secondary
            c.image = ProjectTile.image(name: group.hasProject ? group.name : "Home", colorIndex: group.colorIndex, side: (16 * TypeScale.factor).rounded())
            c.imageToTextPadding = 8
            c.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 10, leading: 20, bottom: 2, trailing: 20)
            c.secondaryText = group.cards.count == 1 ? nil : "\(group.cards.count)"
            c.prefersSideBySideTextAndSecondaryText = true
            c.secondaryTextProperties.font = Fonts.ui(.sansMedium, TypeScale.size(12))
            c.secondaryTextProperties.color = Palette.tertiary
            cell.contentConfiguration = c
            var bg = UIBackgroundConfiguration.clear()
            bg.backgroundColor = .clear
            cell.backgroundConfiguration = bg
            cell.accessibilityTraits = .header
        }
        let cardReg = UICollectionView.CellRegistration<BoardCardCell, Item> { [weak self] cell, _, item in
            guard let self, case let .card(id) = item, let card = self.cards[id] else { return }
            cell.configure(card, now: Date())
        }
        let headerReg = UICollectionView.SupplementaryRegistration<UICollectionViewListCell>(elementKind: UICollectionView.elementKindSectionHeader) { [weak self] view, _, path in
            self?.configureHeader(view, section: path.section)
        }
        dataSource = UICollectionViewDiffableDataSource(collectionView: collectionView) { cv, path, item in
            switch item {
            case .totals: cv.dequeueConfiguredReusableCell(using: totalsReg, for: path, item: item)
            case .project: cv.dequeueConfiguredReusableCell(using: projectReg, for: path, item: item)
            case .card: cv.dequeueConfiguredReusableCell(using: cardReg, for: path, item: item)
            }
        }
        dataSource.supplementaryViewProvider = { cv, _, path in
            cv.dequeueConfiguredReusableSupplementary(using: headerReg, for: path)
        }
        token = app.observe { [weak self] in self?.reload(animated: true) }
        reload(animated: false)
        registerForTraitChanges([UITraitPreferredContentSizeCategory.self]) { (self: BoardViewController, _) in
            self.reconfigureAll()
        }
        let refresh = UIRefreshControl()
        refresh.addAction(UIAction { [weak self, weak refresh] _ in
            guard let self else { return refresh?.endRefreshing() ?? () }
            self.app.refresh { refresh?.endRefreshing() }
        }, for: .valueChanged)
        collectionView.refreshControl = refresh
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        reload(animated: false)
        // "Working · 4m", "back in 47m" age between registry events.
        clock = Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { [weak self] _ in self?.reload(animated: false) }
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        clock?.invalidate()
        clock = nil
    }

    private func reconfigureAll() {
        var s = dataSource.snapshot()
        s.reconfigureItems(s.itemIdentifiers)
        dataSource.apply(s, animatingDifferences: false)
    }

    private func reload(animated: Bool) {
        let next = BoardModel.build(rows: app.boardRows, projects: app.projectViews, devices: app.rawDevices, attention: app.inbox.items, now: Date())
        let old = dataSource.snapshot()
        var s = NSDiffableDataSourceSnapshot<String, Item>()
        var nextCards: [String: BoardModel.Card] = [:]
        var changed: [Item] = []
        if !next.isEmpty {
            s.appendSections(["#totals"])
            s.appendItems([.totals], toSection: "#totals")
            if next.totals != model.totals, old.indexOfItem(.totals) != nil { changed.append(.totals) }
        }
        for device in next.devices {
            s.appendSections([device.id])
            for project in device.projects {
                let header = Item.project(device: device.id, key: project.key)
                s.appendItems([header], toSection: device.id)
                if old.indexOfItem(header) != nil { changed.append(header) }
                for card in project.cards where nextCards[card.id] == nil {
                    nextCards[card.id] = card
                    s.appendItems([.card(card.id)], toSection: device.id)
                    if let prev = cards[card.id], prev != card, old.indexOfItem(.card(card.id)) != nil { changed.append(.card(card.id)) }
                }
            }
        }
        model = next
        cards = nextCards
        s.reconfigureItems(changed)
        dataSource.apply(s, animatingDifferences: animated && view.window != nil)
        // Device headers carry counts and online state.
        for path in collectionView.indexPathsForVisibleSupplementaryElements(ofKind: UICollectionView.elementKindSectionHeader) {
            if let header = collectionView.supplementaryView(forElementKind: UICollectionView.elementKindSectionHeader, at: path) as? UICollectionViewListCell {
                configureHeader(header, section: path.section)
            }
        }
        var empty: UIContentUnavailableConfiguration?
        if next.isEmpty {
            var c = UIContentUnavailableConfiguration.empty()
            c.image = UIImage(systemName: "square.grid.2x2")
            c.text = "No agents running"
            c.secondaryText = "Sessions working, waiting on you or finished today show up here, from every device."
            c.textProperties.font = Fonts.ui(.sansSemibold, TypeScale.size(18))
            c.secondaryTextProperties.font = Fonts.ui(.sans, TypeScale.size(14))
            c.secondaryTextProperties.color = Palette.secondary
            c.imageProperties.tintColor = Palette.tertiary
            empty = c
        }
        contentUnavailableConfiguration = empty
        collectionView.isHidden = empty != nil
    }

    private func configureHeader(_ view: UICollectionViewListCell, section: Int) {
        guard let id = dataSource.sectionIdentifier(for: section),
              let device = model.devices.first(where: { $0.id == id }) else {
            view.contentConfiguration = nil
            return
        }
        var c = UIListContentConfiguration.header()
        c.text = device.name
        c.textProperties.font = Fonts.ui(.sansSemibold, TypeScale.size(17))
        c.textProperties.color = Palette.text
        c.secondaryText = (device.online ? "Online" : "Offline") + " · " + (device.count == 1 ? "1 agent" : "\(device.count) agents")
        c.secondaryTextProperties.font = Fonts.ui(.sans, TypeScale.size(12.5))
        c.secondaryTextProperties.color = device.online ? Palette.success : Palette.tertiary
        c.image = UIImage(systemName: device.online ? "desktopcomputer" : "desktopcomputer.trianglebadge.exclamationmark")
        c.imageProperties.tintColor = device.online ? Palette.secondary : Palette.tertiary
        c.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 14, leading: 20, bottom: 2, trailing: 20)
        view.contentConfiguration = c
        var bg = UIBackgroundConfiguration.clear()
        bg.backgroundColor = Palette.background
        view.backgroundConfiguration = bg
        view.accessibilityTraits = .header
    }

    private func card(_ path: IndexPath) -> BoardModel.Card? {
        guard case let .card(id)? = dataSource.itemIdentifier(for: path) else { return nil }
        return cards[id]
    }

    func collectionView(_ collectionView: UICollectionView, shouldSelectItemAt indexPath: IndexPath) -> Bool {
        card(indexPath) != nil
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: true)
        guard let card = card(indexPath) else { return }
        open(card.id)
    }

    private func open(_ chatId: String) {
        if let split = splitViewController as? SplitRootController, !split.isCollapsed {
            split.openSession(chatId)
            return
        }
        guard let session = SessionViewController(app: app, chatId: chatId) else {
            Toast.show("Couldn't open session. Try again.", in: view.window)
            return
        }
        navigationController?.pushViewController(session, animated: true)
    }

    func collectionView(_ collectionView: UICollectionView, contextMenuConfigurationForItemsAt indexPaths: [IndexPath], point: CGPoint) -> UIContextMenuConfiguration? {
        guard let path = indexPaths.first, let card = card(path) else { return nil }
        return UIContextMenuConfiguration(identifier: nil, previewProvider: nil) { [weak self] _ in
            guard let self else { return nil }
            var children: [UIMenuElement] = [
                UIAction(title: "Open Chat", image: UIImage(systemName: "arrow.up.right")) { _ in self.open(card.id) },
            ]
            if let key = card.approvableKey, let item = self.app.inbox.item(key: key) {
                children.append(UIAction(title: "Approve Low-Risk Request", image: UIImage(systemName: "checkmark.shield")) { _ in
                    Task { @MainActor in
                        if let error = await self.app.respond(item, approve: true) { Toast.show(error, in: self.view.window) }
                    }
                })
            }
            children.append(self.app.lifecycleMenu(for: card.id))
            return UIMenu(children: children)
        }
    }
}

/// The totals strip: working, needs you, limited, finished today.
struct BoardTotalsConfiguration: UIContentConfiguration {
    var totals: BoardModel.Totals

    func makeContentView() -> UIView & UIContentView { BoardTotalsView(configuration: self) }
    func updated(for state: UIConfigurationState) -> BoardTotalsConfiguration { self }
}

final class BoardTotalsView: UIView, UIContentView {
    private let stack = UIStackView()
    var configuration: UIContentConfiguration {
        didSet { apply() }
    }

    init(configuration: BoardTotalsConfiguration) {
        self.configuration = configuration
        super.init(frame: .zero)
        stack.axis = .horizontal
        stack.spacing = 8
        stack.distribution = .fillEqually
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
        ])
        apply()
    }

    required init?(coder: NSCoder) { fatalError() }

    private func apply() {
        guard let c = configuration as? BoardTotalsConfiguration else { return }
        stack.arrangedSubviews.forEach { $0.removeFromSuperview() }
        let tiles: [(Int, String, UIColor)] = [
            (c.totals.working, "Working", BoardModel.Status.working.tint),
            (c.totals.needsYou, "Needs you", BoardModel.Status.needsYou.tint),
            (c.totals.limited, "Limited", BoardModel.Status.limited.tint),
            (c.totals.finishedToday, "Done today", BoardModel.Status.finished.tint),
        ]
        for (n, label, tint) in tiles {
            let tile = UIView()
            tile.backgroundColor = Palette.elevated
            tile.layer.cornerRadius = 14
            tile.layer.cornerCurve = .continuous
            let number = UILabel()
            number.text = "\(n)"
            number.font = Fonts.ui(.sansSemibold, TypeScale.size(20))
            number.textColor = n > 0 ? tint : Palette.tertiary
            let text = UILabel()
            text.text = label
            text.font = Fonts.ui(.sansMedium, TypeScale.size(11.5))
            text.textColor = Palette.secondary
            text.adjustsFontSizeToFitWidth = true
            text.minimumScaleFactor = 0.7
            let v = UIStackView(arrangedSubviews: [number, text])
            v.axis = .vertical
            v.spacing = 1
            v.translatesAutoresizingMaskIntoConstraints = false
            tile.addSubview(v)
            NSLayoutConstraint.activate([
                v.leadingAnchor.constraint(equalTo: tile.leadingAnchor, constant: 10),
                v.trailingAnchor.constraint(lessThanOrEqualTo: tile.trailingAnchor, constant: -6),
                v.topAnchor.constraint(equalTo: tile.topAnchor, constant: 8),
                v.bottomAnchor.constraint(equalTo: tile.bottomAnchor, constant: -8),
            ])
            tile.isAccessibilityElement = true
            tile.accessibilityLabel = "\(n) \(label)"
            stack.addArrangedSubview(tile)
        }
    }
}

/// One agent card.
///
///   [mark]  Title of the session ·········· (● Working · 4m)
///           What it is doing or waiting on, up to two lines
///           ⎇ branch   ◇ 3 subagents
final class BoardCardCell: UICollectionViewListCell {
    private let mark = UIImageView()
    private let title = UILabel()
    private let pill = PaddedLabel()
    private let glyph = StatusGlyph()
    private let activity = UILabel()
    private let meta = UILabel()
    private let plate = UIView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        plate.backgroundColor = Palette.elevated
        plate.layer.cornerRadius = 16
        plate.layer.cornerCurve = .continuous
        mark.contentMode = .scaleAspectFit
        mark.tintColor = Palette.text
        title.numberOfLines = 2
        title.textColor = Palette.text
        pill.layer.cornerRadius = 9
        pill.layer.cornerCurve = .continuous
        pill.clipsToBounds = true
        pill.insets = UIEdgeInsets(top: 3, left: 22, bottom: 3, right: 9)
        pill.setContentCompressionResistancePriority(.required, for: .horizontal)
        pill.setContentHuggingPriority(.required, for: .horizontal)
        activity.numberOfLines = 2
        activity.textColor = Palette.secondary
        meta.textColor = Palette.subline
        meta.numberOfLines = 1
        for v in [plate, mark, title, pill, glyph, activity, meta] as [UIView] {
            v.translatesAutoresizingMaskIntoConstraints = false
        }
        contentView.addSubview(plate)
        for v in [mark, title, pill, activity, meta] as [UIView] { plate.addSubview(v) }
        pill.addSubview(glyph)
        let metaBottom = meta.bottomAnchor.constraint(equalTo: plate.bottomAnchor, constant: -12)
        NSLayoutConstraint.activate([
            plate.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 12),
            plate.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -12),
            plate.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 4),
            plate.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -4),
            mark.leadingAnchor.constraint(equalTo: plate.leadingAnchor, constant: 14),
            mark.topAnchor.constraint(equalTo: plate.topAnchor, constant: 15),
            mark.widthAnchor.constraint(equalToConstant: 18),
            mark.heightAnchor.constraint(equalToConstant: 18),
            title.leadingAnchor.constraint(equalTo: mark.trailingAnchor, constant: 11),
            title.topAnchor.constraint(equalTo: plate.topAnchor, constant: 12),
            pill.leadingAnchor.constraint(greaterThanOrEqualTo: title.trailingAnchor, constant: 8),
            pill.trailingAnchor.constraint(equalTo: plate.trailingAnchor, constant: -12),
            pill.topAnchor.constraint(equalTo: plate.topAnchor, constant: 12),
            glyph.leadingAnchor.constraint(equalTo: pill.leadingAnchor, constant: 7),
            glyph.centerYAnchor.constraint(equalTo: pill.centerYAnchor),
            glyph.widthAnchor.constraint(equalToConstant: 12),
            glyph.heightAnchor.constraint(equalToConstant: 12),
            activity.leadingAnchor.constraint(equalTo: title.leadingAnchor),
            activity.trailingAnchor.constraint(equalTo: plate.trailingAnchor, constant: -14),
            activity.topAnchor.constraint(equalTo: title.bottomAnchor, constant: 4),
            meta.leadingAnchor.constraint(equalTo: title.leadingAnchor),
            meta.trailingAnchor.constraint(equalTo: plate.trailingAnchor, constant: -14),
            meta.topAnchor.constraint(equalTo: activity.bottomAnchor, constant: 6),
            metaBottom,
        ])
        var bg = UIBackgroundConfiguration.clear()
        bg.backgroundColor = .clear
        backgroundConfiguration = bg
    }

    required init?(coder: NSCoder) { fatalError() }

    override func updateConfiguration(using state: UICellConfigurationState) {
        var bg = UIBackgroundConfiguration.clear()
        bg.backgroundColor = .clear
        backgroundConfiguration = bg
        plate.backgroundColor = state.isHighlighted || state.isSelected ? Palette.controlFill : Palette.elevated
    }

    func configure(_ card: BoardModel.Card, now: Date) {
        mark.image = BrandMarks.image(for: card.harness ?? "claude-code", side: 18)
        title.font = Fonts.ui(card.fresh ? .sansSemibold : .sansMedium, TypeScale.size(15.5))
        title.text = card.title
        let tint = card.status.tint
        pill.text = card.statusLabel
        pill.font = Fonts.ui(.sansSemibold, TypeScale.size(11.5))
        pill.textColor = tint
        pill.backgroundColor = tint.withAlphaComponent(0.13)
        switch card.status {
        case .working: glyph.kind = .spinner
        case .finished: glyph.kind = .check(tint)
        default: glyph.kind = .dot(tint)
        }
        activity.font = Fonts.ui(.sans, TypeScale.size(13.5))
        activity.text = card.activity ?? "No messages yet"
        activity.textColor = card.activity == nil ? Palette.tertiary : Palette.secondary
        let metaFont = Fonts.ui(.sans, TypeScale.size(12.5))
        let line = NSMutableAttributedString()
        func icon(_ name: String) {
            guard let image = UIImage(systemName: name, withConfiguration: UIImage.SymbolConfiguration(pointSize: 11, weight: .medium))?.withTintColor(Palette.subline, renderingMode: .alwaysOriginal) else { return }
            let a = NSTextAttachment(image: image)
            a.bounds = CGRect(x: 0, y: (metaFont.capHeight - image.size.height) / 2, width: image.size.width, height: image.size.height)
            line.append(NSAttributedString(attachment: a))
        }
        if let b = card.branch {
            icon("arrow.triangle.branch")
            line.append(NSAttributedString(string: " " + b, attributes: [.font: metaFont, .foregroundColor: Palette.subline]))
        }
        if card.subagents > 0 {
            if line.length > 0 { line.append(NSAttributedString(string: "    ")) }
            icon("person.2")
            let n = card.subagents
            line.append(NSAttributedString(string: " \(n) subagent\(n == 1 ? "" : "s")", attributes: [.font: metaFont, .foregroundColor: Palette.accent]))
        }
        meta.attributedText = line
        meta.isHidden = line.length == 0
        var a11y = [card.title, card.statusLabel]
        if let act = card.activity { a11y.append(act) }
        if let b = card.branch { a11y.append("branch \(b)") }
        if card.subagents > 0 { a11y.append("\(card.subagents) subagents") }
        accessibilityLabel = a11y.joined(separator: ", ")
        accessibilityHint = "Double-tap to open the chat."
        accessibilityIdentifier = "board-\(card.id)"
        isAccessibilityElement = true
    }
}
