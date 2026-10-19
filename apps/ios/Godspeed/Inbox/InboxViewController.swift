import UIKit

/// "Needs you": one list of everything waiting on you across your devices,
/// most urgent first (high-risk approvals, questions, other approvals,
/// failures, limits, finished work). Approvals show the exact command or
/// diff in their risk colour; swipe right to approve, left to decline (or
/// dismiss); tap to open the chat. "Approve all low-risk" answers every
/// low-risk approval on every host at once.
final class InboxViewController: UIViewController, UICollectionViewDelegate {
    private let app: AppModel
    private var collectionView: UICollectionView!
    private var dataSource: UICollectionViewDiffableDataSource<Int, String>!
    private var rows: [String: AttentionItemView] = [:]
    private var token: AnyObject?
    private var clock: Timer?
    private static let headerKey = "#header"

    init(app: AppModel) {
        self.app = app
        super.init(nibName: nil, bundle: nil)
        title = "Needs You"
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.largeTitleDisplayMode = .always
        var config = UICollectionLayoutListConfiguration(appearance: .plain)
        config.backgroundColor = .clear
        config.showsSeparators = false
        config.leadingSwipeActionsConfigurationProvider = { [weak self] path in self?.swipe(path, leading: true) }
        config.trailingSwipeActionsConfigurationProvider = { [weak self] path in self?.swipe(path, leading: false) }
        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: UICollectionViewCompositionalLayout.list(using: config))
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.backgroundColor = .clear
        collectionView.delegate = self
        collectionView.accessibilityIdentifier = "inbox-list"
        view.addSubview(collectionView)

        let cell = UICollectionView.CellRegistration<AttentionCell, String> { [weak self] cell, _, key in
            guard let self, let item = self.rows[key] else { return }
            cell.configure(item, now: Date())
            cell.accessibilityCustomActions = self.customActions(item)
        }
        let headerReg = UICollectionView.CellRegistration<InboxHeaderCell, String> { [weak self] cell, _, _ in
            guard let self else { return }
            cell.header.onApproveAll = { [weak self] in self?.approveAll() }
            cell.header.update(self.app.inbox)
        }
        dataSource = UICollectionViewDiffableDataSource(collectionView: collectionView) { cv, path, key in
            key == Self.headerKey
                ? cv.dequeueConfiguredReusableCell(using: headerReg, for: path, item: key)
                : cv.dequeueConfiguredReusableCell(using: cell, for: path, item: key)
        }
        token = app.observe { [weak self] in self?.reload(animated: true) }
        reload(animated: false)
        registerForTraitChanges([UITraitPreferredContentSizeCategory.self]) { (self: InboxViewController, _) in
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
        app.refreshAttention()
        // "2m ago" ages without events.
        clock = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in self?.reconfigureAll() }
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        clock?.invalidate()
        clock = nil
    }

    private func reconfigureAll() {
        guard dataSource != nil else { return }
        var s = dataSource.snapshot()
        s.reconfigureItems(s.itemIdentifiers)
        dataSource.apply(s, animatingDifferences: false)
    }

    private func reload(animated: Bool) {
        let model = app.inbox
        var next: [String: AttentionItemView] = [:]
        var changed: [String] = []
        var s = NSDiffableDataSourceSnapshot<Int, String>()
        s.appendSections([0])
        if !model.items.isEmpty {
            s.appendItems([Self.headerKey])
            if dataSource.snapshot().indexOfItem(Self.headerKey) != nil { changed.append(Self.headerKey) }
        }
        for item in model.items {
            let key = InboxModel.key(item)
            if let old = rows[key], old != item { changed.append(key) }
            next[key] = item
            s.appendItems([key])
        }
        rows = next
        s.reconfigureItems(changed)
        dataSource.apply(s, animatingDifferences: animated && view.window != nil)
        var empty: UIContentUnavailableConfiguration?
        if model.items.isEmpty {
            var c = UIContentUnavailableConfiguration.empty()
            c.image = UIImage(systemName: "checkmark.seal")
            c.text = "Nothing needs you"
            c.secondaryText = "Approvals, questions and finished work from every device land here."
            c.textProperties.font = Fonts.ui(.sansSemibold, TypeScale.size(18))
            c.secondaryTextProperties.font = Fonts.ui(.sans, TypeScale.size(14))
            c.secondaryTextProperties.color = Palette.secondary
            c.imageProperties.tintColor = Palette.tertiary
            empty = c
        }
        contentUnavailableConfiguration = empty
        collectionView.isHidden = empty != nil
    }

    // MARK: Actions

    private func item(_ path: IndexPath) -> AttentionItemView? {
        dataSource.itemIdentifier(for: path).flatMap { rows[$0] }
    }

    private func swipe(_ path: IndexPath, leading: Bool) -> UISwipeActionsConfiguration? {
        guard let item = item(path) else { return nil }
        let swipes = InboxModel.swipes(item)
        guard let action = leading ? swipes.leading : swipes.trailing else { return nil }
        let contextual = UIContextualAction(style: .normal, title: title(action, item)) { [weak self] _, _, done in
            self?.perform(action, item)
            done(true)
        }
        switch action {
        case .approve:
            contextual.image = UIImage(systemName: "checkmark")
            contextual.backgroundColor = Palette.success
        case .decline:
            contextual.image = UIImage(systemName: "xmark")
            contextual.backgroundColor = Palette.danger
        case .dismiss:
            contextual.image = UIImage(systemName: "eye.slash")
            contextual.backgroundColor = Palette.secondary
        case .open:
            contextual.backgroundColor = Palette.accent
        }
        let config = UISwipeActionsConfiguration(actions: [contextual])
        config.performsFirstActionWithFullSwipe = true
        return config
    }

    private func title(_ action: InboxAction, _ item: AttentionItemView) -> String {
        switch action {
        case .approve: item.approval?.approveLabel ?? "Approve"
        case .decline: item.approval?.declineLabel ?? "Decline"
        case .dismiss: "Dismiss"
        case .open: "Open"
        }
    }

    private func perform(_ action: InboxAction, _ item: AttentionItemView) {
        switch action {
        case .approve, .decline:
            let approve = action == .approve
            approve ? UINotificationFeedbackGenerator().notificationOccurred(.success) : UIImpactFeedbackGenerator(style: .medium).impactOccurred()
            Task { @MainActor in
                if let error = await self.app.respond(item, approve: approve) {
                    UINotificationFeedbackGenerator().notificationOccurred(.error)
                    Toast.show(error, in: self.view.window)
                } else {
                    UIAccessibility.post(notification: .announcement, argument: approve ? "Approved" : "Declined")
                }
            }
        case .dismiss:
            Task { @MainActor in await self.app.dismiss(item) }
        case .open:
            open(item)
        }
    }

    private func customActions(_ item: AttentionItemView) -> [UIAccessibilityCustomAction] {
        var out: [UIAccessibilityCustomAction] = []
        func add(_ action: InboxAction) {
            out.append(UIAccessibilityCustomAction(name: title(action, item)) { [weak self] _ in
                self?.perform(action, item)
                return true
            })
        }
        if item.canApprove { add(.approve) }
        if item.canDecline { add(.decline) }
        if item.chatId != nil { add(.open) }
        if item.canDismiss { add(.dismiss) }
        return out
    }

    private func approveAll() {
        let count = app.inbox.lowRiskCount
        guard count > 0 else { return }
        UINotificationFeedbackGenerator().notificationOccurred(.success)
        Task { @MainActor in
            let result = await self.app.approveAllLowRisk()
            var text = result.approved == 1 ? "Approved 1 request" : "Approved \(result.approved) requests"
            if result.skipped > 0 { text += " · \(result.skipped) changed, left for you" }
            if !result.failedDevices.isEmpty { text = result.approved > 0 ? text + " · a host was unreachable" : "Couldn't reach the host" }
            Toast.show(text, in: self.view.window)
        }
    }

    private func open(_ item: AttentionItemView) {
        guard let chatId = item.chatId else { return }
        app.opened(item)
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

    func collectionView(_ collectionView: UICollectionView, shouldSelectItemAt indexPath: IndexPath) -> Bool {
        item(indexPath) != nil
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: true)
        if let item = item(indexPath) { open(item) }
    }

    func collectionView(_ collectionView: UICollectionView, contextMenuConfigurationForItemsAt indexPaths: [IndexPath], point: CGPoint) -> UIContextMenuConfiguration? {
        guard let path = indexPaths.first, let item = item(path) else { return nil }
        return UIContextMenuConfiguration(identifier: nil, previewProvider: nil) { [weak self] _ in
            guard let self else { return nil }
            var actions: [UIMenuElement] = []
            if item.canApprove {
                actions.append(UIAction(title: self.title(.approve, item), image: UIImage(systemName: "checkmark")) { _ in self.perform(.approve, item) })
            }
            if item.canDecline {
                actions.append(UIAction(title: self.title(.decline, item), image: UIImage(systemName: "xmark"), attributes: .destructive) { _ in self.perform(.decline, item) })
            }
            if item.chatId != nil {
                actions.append(UIAction(title: "Open Chat", image: UIImage(systemName: "arrow.up.right")) { _ in self.open(item) })
            }
            if let command = item.approval?.command {
                actions.append(UIAction(title: "Copy Command", image: UIImage(systemName: "doc.on.doc")) { _ in UIPasteboard.general.string = command })
            }
            if item.canDismiss {
                actions.append(UIAction(title: "Dismiss", image: UIImage(systemName: "eye.slash")) { _ in self.perform(.dismiss, item) })
            }
            return UIMenu(children: actions)
        }
    }
}

/// The header row's cell (no highlight, no swipes).
final class InboxHeaderCell: UICollectionViewListCell {
    let header = InboxHeaderView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        header.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(header)
        NSLayoutConstraint.activate([
            header.leadingAnchor.constraint(equalTo: contentView.leadingAnchor),
            header.trailingAnchor.constraint(equalTo: contentView.trailingAnchor),
            header.topAnchor.constraint(equalTo: contentView.topAnchor),
            header.bottomAnchor.constraint(equalTo: contentView.bottomAnchor),
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
    }
}

/// The list's header: what's waiting, and "Approve all low-risk".
final class InboxHeaderView: UIView {
    var onApproveAll: (() -> Void)?
    private let summary = UILabel()
    private let note = UILabel()
    private let button = UIButton(type: .system)
    private let stack = UIStackView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        summary.font = Fonts.ui(.sansMedium, TypeScale.size(14))
        summary.textColor = Palette.secondary
        summary.numberOfLines = 0
        note.font = Fonts.ui(.sans, TypeScale.size(12.5))
        note.textColor = Palette.tertiary
        note.numberOfLines = 0
        var c = UIButton.Configuration.tinted()
        c.cornerStyle = .capsule
        c.baseForegroundColor = Palette.success
        c.baseBackgroundColor = Palette.success
        c.image = UIImage(systemName: "checkmark.circle.fill")
        c.imagePadding = 6
        button.configuration = c
        button.accessibilityIdentifier = "approve-all-low-risk"
        button.addAction(UIAction { [weak self] _ in self?.onApproveAll?() }, for: .touchUpInside)
        stack.axis = .vertical
        stack.alignment = .leading
        stack.spacing = 8
        for v in [summary, button, note] as [UIView] { stack.addArrangedSubview(v) }
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -10),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func update(_ model: InboxModel) {
        var counts: [AttentionKind: Int] = [:]
        for item in model.items { counts[item.kind, default: 0] += 1 }
        let order: [(AttentionKind, String, String)] = [
            (.approval, "approval", "approvals"), (.question, "question", "questions"), (.failed, "failed", "failed"),
            (.limited, "limited", "limited"), (.timedOut, "timed out", "timed out"), (.finished, "finished", "finished"),
        ]
        let parts = order.compactMap { kind, one, many -> String? in
            guard let n = counts[kind], n > 0 else { return nil }
            return "\(n) \(n == 1 ? one : many)"
        }
        summary.text = parts.isEmpty ? nil : parts.joined(separator: " · ")
        summary.isHidden = parts.isEmpty
        let low = model.lowRiskCount
        button.isHidden = low == 0
        var c = button.configuration
        c?.attributedTitle = AttributedString(low == 1 ? "Approve 1 low-risk" : "Approve all \(low) low-risk", attributes: AttributeContainer([.font: Fonts.ui(.sansSemibold, TypeScale.size(14))]))
        button.configuration = c
        button.accessibilityLabel = "Approve all low-risk requests, \(low)"
        let derived = model.items.contains { $0.derived && ($0.kind == .approval || $0.kind == .question) }
        note.text = derived ? "Some hosts can't take answers from the phone yet (Godspeed 0.4 or later on the Mac does). Open the chat to answer there." : nil
        note.isHidden = !derived
    }
}

/// One inbox row: kind glyph in its tint, title and age, where it runs, and
/// for approvals the exact command / diff on a code plate edged in the risk
/// colour.
final class AttentionCell: UICollectionViewListCell {
    private let glyph = UIImageView()
    private let title = UILabel()
    private let age = UILabel()
    private let meta = UILabel()
    private let riskPill = PaddedLabel()
    private let kindLabel = UILabel()
    private let detail = UILabel()
    private let plate = UIView()
    private let edge = UIView()
    private let summary = UILabel()
    private let pillRow = UIStackView()
    private let stack = UIStackView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        glyph.contentMode = .scaleAspectFit
        glyph.setContentHuggingPriority(.required, for: .horizontal)
        title.numberOfLines = 2
        title.textColor = Palette.text
        age.textColor = StatusTone.time
        age.setContentHuggingPriority(.required, for: .horizontal)
        age.setContentCompressionResistancePriority(.required, for: .horizontal)
        meta.textColor = Palette.secondary
        meta.numberOfLines = 1
        riskPill.layer.cornerRadius = 8
        riskPill.layer.cornerCurve = .continuous
        riskPill.clipsToBounds = true
        kindLabel.textColor = Palette.secondary
        pillRow.axis = .horizontal
        pillRow.spacing = 8
        pillRow.alignment = .center
        pillRow.addArrangedSubview(riskPill)
        pillRow.addArrangedSubview(kindLabel)
        detail.numberOfLines = 6
        detail.textColor = Palette.color(.codeText)
        detail.lineBreakMode = .byTruncatingTail
        plate.backgroundColor = Palette.codeBackground
        plate.layer.cornerRadius = 10
        plate.layer.cornerCurve = .continuous
        plate.layer.borderWidth = 1
        plate.layer.borderColor = Palette.codeBorder.cgColor
        plate.clipsToBounds = true
        edge.translatesAutoresizingMaskIntoConstraints = false
        detail.translatesAutoresizingMaskIntoConstraints = false
        plate.addSubview(edge)
        plate.addSubview(detail)
        NSLayoutConstraint.activate([
            edge.leadingAnchor.constraint(equalTo: plate.leadingAnchor),
            edge.topAnchor.constraint(equalTo: plate.topAnchor),
            edge.bottomAnchor.constraint(equalTo: plate.bottomAnchor),
            edge.widthAnchor.constraint(equalToConstant: 3),
            detail.leadingAnchor.constraint(equalTo: plate.leadingAnchor, constant: 12),
            detail.trailingAnchor.constraint(equalTo: plate.trailingAnchor, constant: -10),
            detail.topAnchor.constraint(equalTo: plate.topAnchor, constant: 8),
            detail.bottomAnchor.constraint(equalTo: plate.bottomAnchor, constant: -8),
        ])
        summary.numberOfLines = 2
        summary.textColor = Palette.secondary

        let titleRow = UIStackView(arrangedSubviews: [title, age])
        titleRow.axis = .horizontal
        titleRow.alignment = .firstBaseline
        titleRow.spacing = 8
        stack.axis = .vertical
        stack.spacing = 6
        stack.alignment = .fill
        for v in [titleRow, meta, pillRow, plate, summary] as [UIView] { stack.addArrangedSubview(v) }
        stack.setCustomSpacing(10, after: meta)
        glyph.translatesAutoresizingMaskIntoConstraints = false
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(glyph)
        contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            glyph.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 20),
            glyph.widthAnchor.constraint(equalToConstant: 20),
            glyph.heightAnchor.constraint(equalToConstant: 20),
            glyph.centerYAnchor.constraint(equalTo: title.firstBaselineAnchor, constant: -5),
            stack.leadingAnchor.constraint(equalTo: glyph.trailingAnchor, constant: 14),
            stack.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 12),
            stack.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -12),
        ])
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (cell: AttentionCell, _) in
            cell.plate.layer.borderColor = Palette.codeBorder.cgColor
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    override func updateConfiguration(using state: UICellConfigurationState) {
        var bg = UIBackgroundConfiguration.listCell().updated(for: state)
        bg.backgroundColor = state.isHighlighted || state.isSelected ? Palette.controlFill : .clear
        bg.cornerRadius = 16
        bg.backgroundInsets = NSDirectionalEdgeInsets(top: 1, leading: 8, bottom: 1, trailing: 8)
        backgroundConfiguration = bg
    }

    func configure(_ item: AttentionItemView, now: Date) {
        let tint = InboxModel.tint(item)
        glyph.image = UIImage(systemName: InboxModel.symbol(item.kind), withConfiguration: UIImage.SymbolConfiguration(pointSize: 16, weight: .semibold))
        glyph.tintColor = tint
        title.font = Fonts.ui(.sansSemibold, TypeScale.size(16))
        title.text = item.title
        age.font = Fonts.ui(.sansMedium, TypeScale.size(12.5))
        age.text = UsageFormat.ago(ms: item.createdAtMs, now: now)
        meta.font = Fonts.ui(.sans, TypeScale.size(13))
        meta.text = InboxModel.meta(item)
        meta.isHidden = meta.text?.isEmpty != false

        if let approval = item.approval {
            pillRow.isHidden = false
            if let risk = item.risk {
                riskPill.isHidden = false
                riskPill.text = InboxModel.riskLabel(risk)
                riskPill.font = Fonts.ui(.sansSemibold, TypeScale.size(11.5))
                riskPill.textColor = InboxModel.riskColor(risk)
                riskPill.backgroundColor = InboxModel.riskColor(risk).withAlphaComponent(0.14)
            } else {
                riskPill.isHidden = true
            }
            kindLabel.font = Fonts.ui(.sansMedium, TypeScale.size(12.5))
            kindLabel.text = approval.title
            if let text = InboxModel.detail(item) {
                plate.isHidden = false
                detail.font = Fonts.ui(.mono, TypeScale.size(13))
                detail.text = text
                edge.backgroundColor = InboxModel.riskColor(item.risk)
            } else {
                plate.isHidden = true
            }
            summary.text = approval.reason
            summary.isHidden = approval.reason?.isEmpty != false
        } else {
            pillRow.isHidden = item.kind != .question || !item.derived
            riskPill.isHidden = true
            kindLabel.font = Fonts.ui(.sansMedium, TypeScale.size(12.5))
            kindLabel.text = item.derived ? "Open the chat to answer" : nil
            plate.isHidden = true
            summary.text = item.summary
            summary.isHidden = item.summary.isEmpty
        }
        summary.font = Fonts.ui(.sans, TypeScale.size(14))
        accessibilityLabel = InboxModel.accessibilityLabel(item, now: now)
        accessibilityHint = item.canApprove ? "Swipe right to approve, left to decline. Double-tap to open the chat." : "Double-tap to open the chat."
        accessibilityIdentifier = "inbox-\(item.id)"
        isAccessibilityElement = true
    }
}

/// A label with insets (risk pills).
final class PaddedLabel: UILabel {
    var insets = UIEdgeInsets(top: 2, left: 7, bottom: 2, right: 7)

    override func drawText(in rect: CGRect) {
        super.drawText(in: rect.inset(by: insets))
    }

    override var intrinsicContentSize: CGSize {
        let s = super.intrinsicContentSize
        return CGSize(width: s.width + insets.left + insets.right, height: s.height + insets.top + insets.bottom)
    }
}
