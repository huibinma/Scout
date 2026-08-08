import React, { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

// 与 user_synonyms.rs::UserGroup 对应（BETA-11D）。
interface UserGroup {
  head: string;
  aliases: string[];
}

/**
 * 「我的同义词」面板：自定义同义词词典的增删改 + 导入 / 导出。
 *
 * 原为独立整页 `/synonyms`（UserSynonymsPage），进入后无返回入口——2026-07-07
 * 真机反馈：设置类页面必须收进选项对话框统一管理。本面板即整页内容平移进
 * 「杂项」tab，去掉整页外壳（居中/大标题），复用对话框内滚动。逻辑与命令不变。
 */
export function SynonymsPane() {
  const [groups, setGroups] = useState<UserGroup[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState("");

  // 添加行状态
  const [addHead, setAddHead] = useState("");
  const [addAliases, setAddAliases] = useState("");
  const [addError, setAddError] = useState("");
  const [adding, setAdding] = useState(false);

  // 删除错误状态
  const [deleteError, setDeleteError] = useState("");

  // 编辑行状态
  const [editingHead, setEditingHead] = useState<string | null>(null);
  const [editDraft, setEditDraft] = useState("");
  const [editError, setEditError] = useState("");
  const [saving, setSaving] = useState(false);

  // 导入/导出 textarea
  const [yamlText, setYamlText] = useState("");
  const [ioMsg, setIoMsg] = useState("");
  const [ioError, setIoError] = useState(false);

  useEffect(() => {
    invoke<UserGroup[]>("get_user_synonyms")
      .then((list) => {
        setGroups(list);
        setLoading(false);
      })
      .catch((err) => {
        console.error("get_user_synonyms:", err);
        setLoadError(String(err));
        setLoading(false);
      });
  }, []);

  // 将 aliases 输入文字按逗号（英文/中文）或空白拆分，过滤空串
  const parseAliases = (raw: string): string[] =>
    raw
      .split(/[，,\s]+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);

  const handleAdd = async () => {
    const head = addHead.trim();
    const aliases = parseAliases(addAliases);
    if (!head) {
      setAddError("主词不能为空");
      return;
    }
    setAdding(true);
    setAddError("");
    try {
      const updated = await invoke<UserGroup[]>("add_user_synonym", {
        head,
        aliases,
      });
      setGroups(updated);
      setAddHead("");
      setAddAliases("");
    } catch (err) {
      setAddError(String(err));
    } finally {
      setAdding(false);
    }
  };

  const handleDelete = async (head: string) => {
    setDeleteError("");
    try {
      const updated = await invoke<UserGroup[]>("delete_user_synonym", { head });
      setGroups(updated);
    } catch (err) {
      setDeleteError(String(err));
    }
  };

  const handleEditStart = (g: UserGroup) => {
    setEditingHead(g.head);
    setEditDraft(g.aliases.join(", "));
    setEditError("");
  };

  const handleEditCancel = () => {
    setEditingHead(null);
    setEditDraft("");
    setEditError("");
  };

  const handleEditSave = async (head: string) => {
    const aliases = parseAliases(editDraft);
    setSaving(true);
    setEditError("");
    try {
      const updated = await invoke<UserGroup[]>("update_user_synonym", {
        head,
        aliases,
      });
      setGroups(updated);
      setEditingHead(null);
      setEditDraft("");
    } catch (err) {
      setEditError(String(err));
    } finally {
      setSaving(false);
    }
  };

  const handleExport = async () => {
    setIoMsg("");
    setIoError(false);
    try {
      const text = await invoke<string>("export_user_synonyms");
      setYamlText(text);
      setIoMsg("已导出到下方文本框");
    } catch (err) {
      setIoMsg(String(err));
      setIoError(true);
    }
  };

  const handleImport = async () => {
    setIoMsg("");
    setIoError(false);
    try {
      const updated = await invoke<UserGroup[]>("import_user_synonyms", {
        yamlText,
      });
      setGroups(updated);
      setIoMsg(`导入成功，共 ${updated.length} 组`);
    } catch (err) {
      setIoMsg(String(err));
      setIoError(true);
    }
  };

  return (
    <div className="prefs-form">
      <div className="prefs-field">
        <label className="prefs-label">我的同义词</label>
        <p className="prefs-hint">
          为常用词添加别名，搜索时自动扩展。例如：主词「音乐」→ 别名「歌曲、song」。
          仅保存在本机，可导入 / 导出。
        </p>
      </div>

      {/* 添加新组 */}
      <div className="prefs-field">
        <label className="prefs-label">添加同义词组</label>
        <div
          style={{
            display: "flex",
            gap: "8px",
            flexWrap: "wrap",
            alignItems: "flex-start",
          }}
        >
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "4px",
              flex: "0 0 140px",
            }}
          >
            <label style={labelStyle}>主词</label>
            <input
              type="text"
              value={addHead}
              onChange={(e) => setAddHead(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleAdd();
              }}
              placeholder="例：音乐"
              style={inputStyle}
            />
          </div>
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "4px",
              flex: "1 1 200px",
            }}
          >
            <label style={labelStyle}>别名（逗号分隔）</label>
            <input
              type="text"
              value={addAliases}
              onChange={(e) => setAddAliases(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleAdd();
              }}
              placeholder="例：歌曲, song, music"
              style={inputStyle}
            />
          </div>
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "4px",
              justifyContent: "flex-end",
            }}
          >
            <label style={{ ...labelStyle, visibility: "hidden" }}>添加</label>
            <button
              type="button"
              onClick={handleAdd}
              disabled={adding}
              className="prefs-btn primary"
            >
              {adding ? "添加中…" : "添加"}
            </button>
          </div>
        </div>
        {addError && (
          <p style={{ fontSize: "13px", color: "var(--status-err-fg)", marginTop: "8px" }}>
            {addError}
          </p>
        )}
      </div>

      {/* 同义词组列表 */}
      <div className="prefs-field">
        <label className="prefs-label">已有同义词组</label>
        {loading ? (
          <p className="prefs-status">读取中…</p>
        ) : loadError ? (
          <p style={{ fontSize: "13px", color: "var(--status-err-fg)" }}>加载失败：{loadError}</p>
        ) : groups.length === 0 ? (
          <p className="prefs-status">暂无同义词组。添加后将在这里显示。</p>
        ) : (
          <div
            style={{
              border: "1px solid var(--border)",
              borderRadius: "8px",
              overflow: "hidden",
            }}
          >
            <table
              style={{
                width: "100%",
                fontSize: "13px",
                borderCollapse: "collapse",
              }}
            >
              <thead>
                <tr style={{ background: "var(--header-bg)", textAlign: "left" }}>
                  <th style={thStyle}>主词</th>
                  <th style={thStyle}>别名</th>
                  <th style={{ ...thStyle, width: "120px" }}></th>
                </tr>
              </thead>
              <tbody>
                {groups.map((g, i) => {
                  const isEditing = editingHead === g.head;
                  return (
                    <tr
                      key={g.head}
                      style={{
                        borderTop: i > 0 ? "1px solid var(--border)" : undefined,
                      }}
                    >
                      <td
                        style={{
                          ...tdStyle,
                          fontWeight: 500,
                          whiteSpace: "nowrap",
                        }}
                      >
                        {g.head}
                      </td>
                      <td
                        style={{
                          ...tdStyle,
                          color: "var(--muted)",
                          wordBreak: "break-word",
                        }}
                      >
                        {isEditing ? (
                          <div
                            style={{
                              display: "flex",
                              flexDirection: "column",
                              gap: "4px",
                            }}
                          >
                            <input
                              type="text"
                              value={editDraft}
                              onChange={(e) => setEditDraft(e.target.value)}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") handleEditSave(g.head);
                                if (e.key === "Escape") handleEditCancel();
                              }}
                              autoFocus
                              placeholder="别名（逗号分隔）"
                              style={{
                                ...inputStyle,
                                fontSize: "12px",
                                padding: "4px 8px",
                              }}
                            />
                            {editError && (
                              <span style={{ fontSize: "12px", color: "var(--status-err-fg)" }}>
                                {editError}
                              </span>
                            )}
                          </div>
                        ) : (
                          g.aliases.join(", ")
                        )}
                      </td>
                      <td
                        style={{
                          ...tdStyle,
                          textAlign: "center",
                          whiteSpace: "nowrap",
                        }}
                      >
                        {isEditing ? (
                          <span style={{ display: "inline-flex", gap: "4px" }}>
                            <button
                              type="button"
                              onClick={() => handleEditSave(g.head)}
                              disabled={saving}
                              style={saveBtnStyle(saving)}
                            >
                              {saving ? "保存中…" : "保存"}
                            </button>
                            <button
                              type="button"
                              onClick={handleEditCancel}
                              disabled={saving}
                              style={cancelBtnStyle}
                            >
                              取消
                            </button>
                          </span>
                        ) : (
                          <span style={{ display: "inline-flex", gap: "4px" }}>
                            <button
                              type="button"
                              onClick={() => handleEditStart(g)}
                              disabled={editingHead !== null}
                              style={editBtnStyle(editingHead !== null)}
                              title={`编辑"${g.head}"组别名`}
                            >
                              编辑
                            </button>
                            <button
                              type="button"
                              onClick={() => handleDelete(g.head)}
                              disabled={editingHead !== null}
                              style={{
                                ...deleteBtnStyle,
                                opacity: editingHead !== null ? 0.4 : 1,
                                cursor:
                                  editingHead !== null
                                    ? "not-allowed"
                                    : "pointer",
                              }}
                              title={`删除"${g.head}"组`}
                            >
                              删除
                            </button>
                          </span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {deleteError && (
          <p style={{ fontSize: "13px", color: "var(--status-err-fg)", marginTop: "8px" }}>
            删除失败：{deleteError}
          </p>
        )}
      </div>

      {/* 导入 / 导出 */}
      <div className="prefs-field">
        <details>
          <summary
            style={{
              cursor: "pointer",
              fontSize: "14px",
              fontWeight: 500,
              color: "var(--muted)",
              userSelect: "none",
              marginBottom: "12px",
            }}
          >
            导入 / 导出 YAML
          </summary>
          <div style={{ marginTop: "12px" }}>
            <textarea
              value={yamlText}
              onChange={(e) => setYamlText(e.target.value)}
              rows={10}
              placeholder="YAML 格式的同义词数据将在此显示，也可在此粘贴后导入"
              style={{
                width: "100%",
                boxSizing: "border-box",
                fontFamily: "monospace",
                fontSize: "12px",
                padding: "8px",
                borderRadius: "4px",
                border: "1px solid var(--border)",
                resize: "vertical",
                color: "var(--fg)",
                background: "var(--header-bg)",
              }}
            />
            <div
              style={{
                display: "flex",
                gap: "8px",
                marginTop: "8px",
                flexWrap: "wrap",
              }}
            >
              <button
                type="button"
                onClick={handleExport}
                className="prefs-btn"
              >
                导出到下方
              </button>
              <button
                type="button"
                onClick={handleImport}
                disabled={!yamlText.trim()}
                className="prefs-btn"
              >
                从下方导入
              </button>
            </div>
            {ioMsg && (
              <p
                style={{
                  fontSize: "13px",
                  color: ioError ? "var(--status-err-fg)" : "var(--status-ok-fg)",
                  marginTop: "8px",
                }}
              >
                {ioMsg}
              </p>
            )}
          </div>
        </details>
      </div>
    </div>
  );
}

// ---- 局部样式辅助（自 UserSynonymsPage 平移；表格/编辑行细节样式对话框内复用） ----

const labelStyle: React.CSSProperties = {
  fontSize: "12px",
  color: "var(--muted)",
};

const inputStyle: React.CSSProperties = {
  padding: "7px 10px",
  borderRadius: "4px",
  border: "1px solid var(--border)",
  fontSize: "13px",
  fontFamily: "inherit",
  color: "var(--fg)",
  background: "var(--bg)",
  width: "100%",
  boxSizing: "border-box",
};

const thStyle: React.CSSProperties = {
  padding: "8px 12px",
  fontWeight: 600,
  color: "var(--muted)",
  fontSize: "12px",
};

const tdStyle: React.CSSProperties = {
  padding: "8px 12px",
};

const deleteBtnStyle: React.CSSProperties = {
  padding: "3px 10px",
  borderRadius: "4px",
  border: "1px solid rgba(239, 68, 68, 0.3)",
  background: "var(--status-err-bg)",
  cursor: "pointer",
  fontSize: "12px",
  color: "var(--status-err-fg)",
  fontFamily: "inherit",
};

function editBtnStyle(disabled: boolean): React.CSSProperties {
  return {
    padding: "3px 10px",
    borderRadius: "4px",
    border: "1px solid rgba(240, 93, 50, 0.3)",
    background: "var(--accent-soft)",
    cursor: disabled ? "not-allowed" : "pointer",
    fontSize: "12px",
    color: disabled ? "var(--subtle)" : "var(--accent)",
    fontFamily: "inherit",
    opacity: disabled ? 0.4 : 1,
  };
}

function saveBtnStyle(disabled: boolean): React.CSSProperties {
  return {
    padding: "3px 10px",
    borderRadius: "4px",
    border: "none",
    background: disabled ? "var(--subtle)" : "#1c1917",
    color: "#fff",
    cursor: disabled ? "not-allowed" : "pointer",
    fontSize: "12px",
    fontFamily: "inherit",
    whiteSpace: "nowrap",
  };
}

const cancelBtnStyle: React.CSSProperties = {
  padding: "3px 10px",
  borderRadius: "4px",
  border: "1px solid var(--border)",
  background: "var(--bg)",
  cursor: "pointer",
  fontSize: "12px",
  color: "var(--muted)",
  fontFamily: "inherit",
};
