import UIKit

/// Settings → Usage, as on the desktop: each agent login's live plan windows
/// (5-hour, weekly…) with reset countdowns, read from the host it is signed
/// in on; then the host's usage history — tokens and the estimated cost at
/// list API prices — by day (or hour) and by model.
final class UsageViewController: UIViewController {
    private let app: AppModel
    private let scroll = UIScrollView()
    private let stack = UIStackView()
    private let limits = UIStackView()
    private let limitsStatus = UILabel()
    private let historyStack = UIStackView()
    private let rangeControl = UISegmentedControl(items: ["Today", "7 days", "30 days", "90 days"])
    private let metricControl = UISegmentedControl(items: ["Cost", "Tokens"])
    private var deviceId: String?
    private var accounts: [AgentAccountView] = []
    private var history: UsageHistoryView?
    private var historyError: String?
    private var loadingHistory = false
    private var clock: Timer?
    private var indexingPolls = 0

    private static let ranges: [UsageRange] = [.today, .week, .month, .quarter]

    init(app: AppModel) {
        self.app = app
        super.init(nibName: nil, bundle: nil)
        title = "Usage"
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.largeTitleDisplayMode = .always
        scroll.alwaysBounceVertical = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scroll)
        stack.axis = .vertical
        stack.spacing = 14
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: view.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.frameLayoutGuide.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: scroll.frameLayoutGuide.trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 8),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -32),
        ])
        let refresh = UIRefreshControl()
        refresh.addAction(UIAction { [weak self, weak refresh] _ in
            Task { @MainActor in
                await self?.load(force: true)
                refresh?.endRefreshing()
            }
        }, for: .valueChanged)
        scroll.refreshControl = refresh

        limits.axis = .vertical
        limits.spacing = 10
        limitsStatus.font = Fonts.ui(.sans, TypeScale.size(13))
        limitsStatus.textColor = Palette.secondary
        limitsStatus.numberOfLines = 0
        historyStack.axis = .vertical
        historyStack.spacing = 10
        rangeControl.selectedSegmentIndex = 1
        rangeControl.addAction(UIAction { [weak self] _ in self?.reloadHistory() }, for: .valueChanged)
        rangeControl.accessibilityIdentifier = "usage-range"
        metricControl.selectedSegmentIndex = 0
        metricControl.addAction(UIAction { [weak self] _ in self?.renderHistory() }, for: .valueChanged)

        stack.addArrangedSubview(sectionTitle("Limits"))
        stack.addArrangedSubview(limitsStatus)
        stack.addArrangedSubview(limits)
        stack.setCustomSpacing(26, after: limits)
        stack.addArrangedSubview(sectionTitle("History"))
        stack.addArrangedSubview(rangeControl)
        stack.addArrangedSubview(historyStack)

        deviceId = app.usage.hosts.first(where: \.online)?.id ?? app.usage.hosts.first?.id
        updateHostMenu()
        render()
        Task { @MainActor in await self.load(force: false) }
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        // Countdowns tick without refetching (desktop: every 30 s).
        clock = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in self?.renderLimits() }
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        clock?.invalidate()
        clock = nil
    }

    private func updateHostMenu() {
        let hosts = app.usage.hosts
        guard hosts.count > 1 else {
            navigationItem.rightBarButtonItem = nil
            return
        }
        let actions = hosts.map { host in
            UIAction(title: host.name, subtitle: host.online ? "Online" : "Offline", state: host.id == deviceId ? .on : .off) { [weak self] _ in
                self?.deviceId = host.id
                self?.accounts = []
                self?.history = nil
                self?.updateHostMenu()
                self?.render()
                Task { @MainActor in await self?.load(force: false) }
            }
        }
        let item = UIBarButtonItem(image: UIImage(systemName: "desktopcomputer"), menu: UIMenu(title: "Usage on", children: actions))
        item.accessibilityLabel = "Choose device"
        navigationItem.rightBarButtonItem = item
    }

    @MainActor
    private func load(force: Bool) async {
        guard let deviceId else {
            render()
            return
        }
        // The last probe first (instant), then a fresh one when asked.
        accounts = await app.usage.loadAccounts(deviceId: deviceId, force: false)
        renderLimits()
        if force || accounts.isEmpty {
            accounts = await app.usage.loadAccounts(deviceId: deviceId, force: true)
            renderLimits()
        }
        await loadHistory()
    }

    private func reloadHistory() {
        history = nil
        indexingPolls = 0
        renderHistory()
        Task { @MainActor in await self.loadHistory() }
    }

    @MainActor
    private func loadHistory() async {
        guard let deviceId else { return }
        let range = Self.ranges[max(0, rangeControl.selectedSegmentIndex)]
        loadingHistory = true
        renderHistory()
        do {
            let h = try await app.usage.history(deviceId: deviceId, range: range)
            guard range == Self.ranges[max(0, rangeControl.selectedSegmentIndex)] else { return }
            history = h
            historyError = nil
            // The engine is still backfilling: ask again shortly.
            if h.indexing, indexingPolls < 45 {
                indexingPolls += 1
                DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
                    Task { @MainActor in await self?.loadHistory() }
                }
            }
        } catch CoreError.Unsupported {
            historyError = "Usage history needs Godspeed 0.5.1 or later on \(hostName)."
        } catch {
            historyError = AppModel.describe(error)
        }
        loadingHistory = false
        renderHistory()
    }

    private var hostName: String {
        app.usage.hosts.first { $0.id == deviceId }?.name ?? "the Mac"
    }

    // MARK: Rendering

    private func render() {
        renderLimits()
        renderHistory()
    }

    private func sectionTitle(_ text: String) -> UILabel {
        let l = UILabel()
        l.text = text
        l.font = Fonts.ui(.sansSemibold, TypeScale.size(20))
        l.textColor = Palette.text
        l.accessibilityTraits = .header
        return l
    }

    private func renderLimits() {
        limits.arrangedSubviews.forEach { $0.removeFromSuperview() }
        let now = Date()
        if deviceId == nil {
            limitsStatus.text = "No Mac is connected to this account yet."
            limitsStatus.isHidden = false
            return
        }
        let error = deviceId.flatMap { app.usage.accountErrors[$0] }
        if accounts.isEmpty {
            limitsStatus.text = error ?? "Reading plan limits from \(hostName)…"
            limitsStatus.isHidden = false
            return
        }
        limitsStatus.text = "Plan windows of the agent logins on \(hostName)."
        limitsStatus.isHidden = false
        // Active logins first, then by agent.
        let sorted = accounts.sorted { a, b in a.active != b.active ? a.active : a.harnessLabel < b.harnessLabel }
        for account in sorted {
            limits.addArrangedSubview(AccountUsageCard(account: account, now: now))
        }
    }

    private func renderHistory() {
        historyStack.arrangedSubviews.forEach { $0.removeFromSuperview() }
        guard let h = history else {
            let l = UILabel()
            l.font = Fonts.ui(.sans, TypeScale.size(13))
            l.textColor = Palette.secondary
            l.numberOfLines = 0
            l.text = historyError ?? (loadingHistory || deviceId != nil ? "Loading…" : nil)
            historyStack.addArrangedSubview(l)
            return
        }
        let byCost = metricControl.selectedSegmentIndex == 0
        // Totals.
        let totals = UIStackView()
        totals.axis = .horizontal
        totals.distribution = .fillEqually
        totals.spacing = 8
        totals.addArrangedSubview(statTile(UsageFormat.usd(h.total.costUsd), "Est. cost at API prices"))
        totals.addArrangedSubview(statTile(UsageFormat.tokens(h.total.totalTokens), "Tokens"))
        totals.addArrangedSubview(statTile("\(h.sessions)", h.sessions == 1 ? "Session" : "Sessions"))
        historyStack.addArrangedSubview(totals)
        historyStack.addArrangedSubview(metricControl)
        let chart = UsageBarChart()
        chart.values = h.buckets.map { byCost ? $0.amount.costUsd : Double($0.amount.totalTokens) }
        chart.caption = { [h] i in
            let b = h.buckets[i]
            let date = Date(timeIntervalSince1970: Double(b.startMs) / 1000)
            let f = DateFormatter()
            f.setLocalizedDateFormatFromTemplate(h.bucket == .hour ? "j" : "MMMd")
            return f.string(from: date)
        }
        let peak = chart.values.enumerated().max { $0.element < $1.element }
        chart.accessibilityLabel = "Usage by \(h.bucket == .hour ? "hour" : h.bucket == .week ? "week" : "day")"
        chart.accessibilityValue = peak.map { "Peak \(byCost ? UsageFormat.usd($0.element) : UsageFormat.tokens(UInt64($0.element))) on \(chart.caption?($0.offset) ?? "")" }
        chart.heightAnchor.constraint(equalToConstant: 150).isActive = true
        historyStack.addArrangedSubview(chart)
        if !h.byModel.isEmpty {
            let title = UILabel()
            title.text = "By model"
            title.font = Fonts.ui(.sansSemibold, TypeScale.size(15))
            title.textColor = Palette.text
            historyStack.setCustomSpacing(16, after: chart)
            historyStack.addArrangedSubview(title)
            for slice in h.byModel.prefix(8) {
                historyStack.addArrangedSubview(sliceRow(slice))
            }
        }
        let foot = UILabel()
        foot.numberOfLines = 0
        foot.font = Fonts.ui(.sans, TypeScale.size(12))
        foot.textColor = Palette.tertiary
        var note = "Estimated at list API prices (checked \(h.pricesChecked)), for subscription logins too. Usage counted on \(hostName)."
        if h.indexing { note = "Still indexing past usage… " + note }
        foot.text = note
        historyStack.addArrangedSubview(foot)
    }

    private func statTile(_ value: String, _ label: String) -> UIView {
        let tile = UIView()
        tile.backgroundColor = Palette.elevated
        tile.layer.cornerRadius = 14
        tile.layer.cornerCurve = .continuous
        let v = UILabel()
        v.text = value
        v.font = Fonts.ui(.sansSemibold, TypeScale.size(19))
        v.textColor = Palette.text
        v.adjustsFontSizeToFitWidth = true
        v.minimumScaleFactor = 0.6
        let l = UILabel()
        l.text = label
        l.font = Fonts.ui(.sans, TypeScale.size(11.5))
        l.textColor = Palette.secondary
        l.numberOfLines = 2
        let s = UIStackView(arrangedSubviews: [v, l])
        s.axis = .vertical
        s.spacing = 2
        s.translatesAutoresizingMaskIntoConstraints = false
        tile.addSubview(s)
        NSLayoutConstraint.activate([
            s.leadingAnchor.constraint(equalTo: tile.leadingAnchor, constant: 12),
            s.trailingAnchor.constraint(equalTo: tile.trailingAnchor, constant: -8),
            s.topAnchor.constraint(equalTo: tile.topAnchor, constant: 10),
            s.bottomAnchor.constraint(equalTo: tile.bottomAnchor, constant: -10),
        ])
        tile.isAccessibilityElement = true
        tile.accessibilityLabel = "\(label): \(value)"
        return tile
    }

    private func sliceRow(_ slice: UsageSliceView) -> UIView {
        let name = UILabel()
        name.text = slice.label
        name.font = Fonts.ui(.sansMedium, TypeScale.size(14))
        name.textColor = Palette.text
        let value = UILabel()
        let cost = slice.amount.unpricedTokens > 0 && slice.amount.unpricedTokens >= slice.amount.totalTokens ? "no price" : UsageFormat.usd(slice.amount.costUsd)
        value.text = "\(UsageFormat.tokens(slice.amount.totalTokens)) · \(cost)"
        value.font = Fonts.ui(.mono, TypeScale.size(12.5))
        value.textColor = Palette.secondary
        value.setContentCompressionResistancePriority(.required, for: .horizontal)
        let row = UIStackView(arrangedSubviews: [name, value])
        row.axis = .horizontal
        row.spacing = 8
        row.isAccessibilityElement = true
        row.accessibilityLabel = "\(slice.label), \(UsageFormat.tokens(slice.amount.totalTokens)) tokens, \(cost)"
        return row
    }
}

/// One agent login's plan windows.
final class AccountUsageCard: UIView {
    init(account: AgentAccountView, now: Date) {
        super.init(frame: .zero)
        backgroundColor = Palette.elevated
        layer.cornerRadius = 16
        layer.cornerCurve = .continuous
        let mark = UIImageView(image: BrandMarks.image(for: account.harness, side: 20))
        mark.tintColor = Palette.text
        mark.setContentHuggingPriority(.required, for: .horizontal)
        let name = UILabel()
        name.text = account.harnessLabel
        name.font = Fonts.ui(.sansSemibold, TypeScale.size(15.5))
        name.textColor = Palette.text
        let who = UILabel()
        who.text = [account.email ?? account.displayName, account.planLabel].compactMap { $0 }.joined(separator: " · ")
        who.font = Fonts.ui(.sans, TypeScale.size(12.5))
        who.textColor = Palette.secondary
        let names = UIStackView(arrangedSubviews: [name, who])
        names.axis = .vertical
        names.spacing = 1
        let badge = PaddedLabel()
        badge.text = account.active ? "In use" : "Saved"
        badge.font = Fonts.ui(.sansSemibold, TypeScale.size(11))
        badge.textColor = account.active ? Palette.accent : Palette.tertiary
        badge.backgroundColor = account.active ? Palette.accentSoft : Palette.controlFill
        badge.layer.cornerRadius = 8
        badge.layer.cornerCurve = .continuous
        badge.clipsToBounds = true
        badge.setContentHuggingPriority(.required, for: .horizontal)
        let top = UIStackView(arrangedSubviews: [mark, names, badge])
        top.axis = .horizontal
        top.spacing = 10
        top.alignment = .center
        let stack = UIStackView(arrangedSubviews: [top])
        stack.axis = .vertical
        stack.spacing = 12
        for w in account.windows {
            stack.addArrangedSubview(UsageMeterView(window: w, now: now))
        }
        if account.windows.isEmpty || account.error != nil {
            let l = UILabel()
            l.numberOfLines = 0
            l.font = Fonts.ui(.sans, TypeScale.size(12.5))
            l.textColor = account.error == nil ? Palette.tertiary : Palette.warning
            l.text = account.error ?? "Usage unavailable for this login"
            stack.addArrangedSubview(l)
        }
        if let at = account.fetchedAtMs {
            let l = UILabel()
            l.font = Fonts.ui(.sans, TypeScale.size(11.5))
            l.textColor = Palette.tertiary
            l.text = "Updated \(UsageFormat.ago(ms: at, now: now))"
            stack.addArrangedSubview(l)
        }
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -14),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 14),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -12),
        ])
        accessibilityIdentifier = "usage-account-\(account.id)"
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// "5-hour  64%" over a bar in the level colour, then "resets in 2h 14m · 3:40 PM".
final class UsageMeterView: UIView {
    static func color(_ level: UsageFormat.Level) -> UIColor {
        switch level {
        case .normal: Palette.accent
        case .warn: Palette.warning
        case .critical: Palette.danger
        }
    }

    init(window: UsageWindowView, now: Date) {
        super.init(frame: .zero)
        let level = UsageFormat.level(window.usedFraction)
        let tint = Self.color(level)
        let label = UILabel()
        label.text = UsageFormat.windowLabel(window.label)
        label.font = Fonts.ui(.sansMedium, TypeScale.size(13.5))
        label.textColor = Palette.text
        let percent = UILabel()
        percent.text = UsageFormat.percent(window.usedFraction)
        percent.font = Fonts.ui(.sansSemibold, TypeScale.size(13.5))
        percent.textColor = level == .normal ? Palette.text : tint
        percent.setContentHuggingPriority(.required, for: .horizontal)
        let row = UIStackView(arrangedSubviews: [label, percent])
        row.axis = .horizontal
        let track = UIView()
        track.backgroundColor = Palette.controlFill
        track.layer.cornerRadius = 3
        track.clipsToBounds = true
        let fill = UIView()
        fill.backgroundColor = tint.withAlphaComponent(level == .normal ? 0.8 : 0.9)
        fill.layer.cornerRadius = 3
        fill.translatesAutoresizingMaskIntoConstraints = false
        track.addSubview(fill)
        track.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            track.heightAnchor.constraint(equalToConstant: 6),
            fill.leadingAnchor.constraint(equalTo: track.leadingAnchor),
            fill.topAnchor.constraint(equalTo: track.topAnchor),
            fill.bottomAnchor.constraint(equalTo: track.bottomAnchor),
            fill.widthAnchor.constraint(equalTo: track.widthAnchor, multiplier: CGFloat(max(0.015, min(1, window.usedFraction)))),
        ])
        let reset = UILabel()
        reset.font = Fonts.ui(.sans, TypeScale.size(12))
        reset.textColor = Palette.secondary
        if let at = window.resetsAtMs {
            reset.text = UsageFormat.resetsIn(ms: at, now: now).capitalizedFirst + " · " + UsageFormat.moment(ms: at, now: now)
        }
        let stack = UIStackView(arrangedSubviews: [row, track, reset])
        stack.axis = .vertical
        stack.spacing = 5
        reset.isHidden = window.resetsAtMs == nil
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        isAccessibilityElement = true
        accessibilityLabel = "\(UsageFormat.windowLabel(window.label)) window"
        accessibilityValue = [UsageFormat.percent(window.usedFraction) + " used", reset.text].compactMap { $0 }.joined(separator: ", ")
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// Bars for a usage series (cost or tokens per bucket), first and last
/// bucket captioned.
final class UsageBarChart: UIView {
    var values: [Double] = [] { didSet { setNeedsDisplay() } }
    var caption: ((Int) -> String)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .clear
        isOpaque = false
        contentMode = .redraw
        isAccessibilityElement = true
        accessibilityTraits = .image
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (chart: UsageBarChart, _) in chart.setNeedsDisplay() }
    }

    required init?(coder: NSCoder) { fatalError() }

    override func draw(_ rect: CGRect) {
        guard !values.isEmpty else { return }
        let captionH: CGFloat = 18
        let chart = bounds.insetBy(dx: 2, dy: 0).inset(by: UIEdgeInsets(top: 4, left: 0, bottom: captionH, right: 0))
        let peak = max(values.max() ?? 0, 1e-9)
        let n = CGFloat(values.count)
        let gap: CGFloat = n > 40 ? 1 : 3
        let w = max(1, (chart.width - gap * (n - 1)) / n)
        Palette.hairline.setFill()
        UIBezierPath(rect: CGRect(x: chart.minX, y: chart.maxY, width: chart.width, height: 1 / max(1, traitCollection.displayScale))).fill()
        for (i, v) in values.enumerated() {
            let h = v <= 0 ? 0 : max(2, CGFloat(v / peak) * chart.height)
            let x = chart.minX + CGFloat(i) * (w + gap)
            let bar = CGRect(x: x, y: chart.maxY - h, width: w, height: h)
            (i == values.count - 1 ? Palette.accent : Palette.accent.withAlphaComponent(0.55)).setFill()
            UIBezierPath(roundedRect: bar, cornerRadius: min(3, w / 2)).fill()
        }
        let attrs: [NSAttributedString.Key: Any] = [.font: Fonts.ui(.sans, 11), .foregroundColor: Palette.tertiary]
        if let first = caption?(0) {
            (first as NSString).draw(at: CGPoint(x: chart.minX, y: chart.maxY + 3), withAttributes: attrs)
        }
        if values.count > 1, let last = caption?(values.count - 1) {
            let size = (last as NSString).size(withAttributes: attrs)
            (last as NSString).draw(at: CGPoint(x: chart.maxX - size.width, y: chart.maxY + 3), withAttributes: attrs)
        }
    }

}

extension String {
    /// "resets in 2h" → "Resets in 2h".
    var capitalizedFirst: String { prefix(1).uppercased() + dropFirst() }
}
