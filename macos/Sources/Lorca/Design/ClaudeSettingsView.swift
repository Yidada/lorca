import AppKit

struct ClaudeSelection {
    var model: String?
    var thinking: String?
}

/// CLI aliases resolve against the account on the assigned Runner.
final class ClaudeSettingsView: NSStackView {
    var onChange: ((ClaudeSelection) -> Void)?
    private var selection: ClaudeSelection

    init(selection: ClaudeSelection = .init()) {
        self.selection = selection
        super.init(frame: .zero)
        orientation = .vertical
        alignment = .leading
        spacing = 0
        translatesAutoresizingMaskIntoConstraints = false
        render()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    private func render() {
        for view in arrangedSubviews { removeArrangedSubview(view); view.removeFromSuperview() }
        var models: [String?] = [nil, "opus", "sonnet", "haiku"]
        if let model = selection.model, !models.contains(model) { models.append(model) }
        let model = PopUpRow(key: L("Model"), items: models.map { $0 ?? L("Runner default") },
                             selected: models.firstIndex(of: selection.model) ?? 0)
        model.onChange = { [weak self] index in
            guard let self, models.indices.contains(index) else { return }
            self.selection.model = models[index]
            self.selection.thinking = nil
            self.changed()
        }
        let levels: [String?] = [nil, "low", "medium", "high", "xhigh", "max"]
        let thinking = PopUpRow(key: L("Thinking"), items: levels.map { $0 ?? L("Runner default") },
                                selected: levels.firstIndex(of: selection.thinking) ?? 0)
        thinking.onChange = { [weak self] index in
            guard let self, levels.indices.contains(index) else { return }
            self.selection.thinking = levels[index]
            self.changed()
        }
        for view in [model, thinking, NoteRow(text: L("Uses Claude Code on this Runner. Sign in there first. Available models and thinking levels depend on its version and account."))] {
            addArrangedSubview(view)
            view.widthAnchor.constraint(equalTo: widthAnchor).isActive = true
        }
    }

    private func changed() { onChange?(selection); render() }
}

