import { useEffect, useState, useCallback } from 'react';
import {
  FileText, Table2, Presentation, Image, FileAudio, File,
  Download, RefreshCw, FolderOpen, Search,
} from 'lucide-react';
import { getFiles, downloadFileUrl, type FileEntry } from '@/lib/api';
import { useT } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { useAgentStore } from '@/lib/store';

interface DeliverableFile {
  name: string;
  path: string;
  size: number;
  ext: string;
  modified?: number;
}

function getFileIcon(ext: string): React.ReactNode {
  switch (ext) {
    case 'pptx': case 'ppt': return <Presentation size={18} className="text-orange-400" />;
    case 'xlsx': case 'xls': case 'csv': return <Table2 size={18} className="text-green-400" />;
    case 'docx': case 'doc': case 'pdf': case 'txt': case 'md':
      return <FileText size={18} className="text-blue-400" />;
    case 'png': case 'jpg': case 'jpeg': case 'webp': case 'svg': case 'gif':
      return <Image size={18} className="text-purple-400" />;
    case 'mp3': case 'wav': case 'mp4': case 'm4a': case 'mkv': case 'webm':
      return <FileAudio size={18} className="text-pink-400" />;
    default: return <File size={18} className="text-muted-foreground" />;
  }
}

function getFileCategory(ext: string): string {
  if (['pptx', 'ppt'].includes(ext)) return 'PPT';
  if (['xlsx', 'xls', 'csv'].includes(ext)) return '表格';
  if (['docx', 'doc', 'pdf', 'txt', 'md'].includes(ext)) return '文档';
  if (['png', 'jpg', 'jpeg', 'webp', 'svg', 'gif'].includes(ext)) return '图片';
  if (['mp3', 'wav', 'mp4', 'm4a', 'mkv', 'webm'].includes(ext)) return '媒体';
  return '文件';
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatTime(ts?: number): string {
  if (!ts) return '';
  const d = new Date(ts * 1000);
  const now = new Date();
  const diff = now.getTime() - d.getTime();
  if (diff < 60000) return '刚刚';
  if (diff < 3600000) return `${Math.floor(diff / 60000)} 分钟前`;
  if (diff < 86400000) return `${Math.floor(diff / 3600000)} 小时前`;
  return d.toLocaleDateString('zh-CN', { month: 'short', day: 'numeric' });
}

const OUTPUT_DIRS = ['media', 'output', 'reports', 'charts', 'generated'];
const OUTPUT_EXTS = ['pptx', 'docx', 'xlsx', 'csv', 'pdf', 'txt', 'md', 'png', 'jpg', 'jpeg', 'svg', 'mp4', 'mp3', 'wav', 'html'];

export function DeliverablesPage() {
  const t = useT();
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const [files, setFiles] = useState<DeliverableFile[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState('');
  const [filter, setFilter] = useState<string>('all');

  const fetchFiles = useCallback(async () => {
    setLoading(true);
    const found: DeliverableFile[] = [];
    for (const dir of OUTPUT_DIRS) {
      try {
        const data = await getFiles(dir, selectedAgentId);
        for (const entry of (data.entries || []) as FileEntry[]) {
          if (entry.is_dir) continue;
          const ext = entry.name.split('.').pop()?.toLowerCase() || '';
          if (OUTPUT_EXTS.includes(ext)) {
            const modTs = entry.modified ? new Date(entry.modified).getTime() / 1000 : undefined;
            found.push({
              name: entry.name,
              path: entry.path,
              size: entry.size || 0,
              ext,
              modified: modTs,
            });
          }
        }
      } catch {
        // Directory may not exist
      }
    }
    // Sort by modified time desc
    found.sort((a, b) => (b.modified || 0) - (a.modified || 0));
    setFiles(found);
    setLoading(false);
  }, [selectedAgentId]);

  useEffect(() => {
    fetchFiles();
  }, [fetchFiles]);

  const categories = ['all', 'PPT', '表格', '文档', '图片', '媒体', '文件'];

  const filtered = files.filter((f) => {
    const matchSearch = !search || f.name.toLowerCase().includes(search.toLowerCase());
    const matchFilter = filter === 'all' || getFileCategory(f.ext) === filter;
    return matchSearch && matchFilter;
  });

  function handleDownload(file: DeliverableFile) {
    const url = downloadFileUrl(file.path, selectedAgentId);
    const a = document.createElement('a');
    a.href = url;
    a.download = file.name;
    a.click();
  }

  return (
    <div className="flex flex-col h-full overflow-y-auto">
      <div className="border-b border-border px-6 py-4 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <FolderOpen size={18} className="text-muted-foreground" />
          <h1 className="text-lg font-semibold">{t('deliverables.title')}</h1>
          <span className="text-xs text-muted-foreground bg-muted px-2 py-0.5 rounded-full">
            {files.length} {t('deliverables.files')}
          </span>
          <span className="text-xs text-muted-foreground">{t('common.agent')}: {selectedAgentId}</span>
        </div>
        <button
          onClick={fetchFiles}
          className="p-2 rounded-lg hover:bg-accent text-muted-foreground"
          title={t('common.refresh')}
        >
          <RefreshCw size={16} className={loading ? 'animate-spin' : ''} />
        </button>
      </div>

      <div className="p-6 space-y-4">
        {/* Search + filter */}
        <div className="flex flex-col sm:flex-row gap-3">
          <div className="relative flex-1">
            <Search size={14} className="absolute left-3 top-1/2 -translate-y-1/2 text-muted-foreground" />
            <input
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder={t('deliverables.searchPlaceholder')}
              className="w-full pl-8 pr-3 py-1.5 text-sm bg-card border border-border rounded-lg outline-none focus:border-primary/50"
            />
          </div>
          <div className="flex gap-1.5 flex-wrap">
            {categories.map((cat) => (
              <button
                key={cat}
                onClick={() => setFilter(cat)}
                className={cn(
                  'px-3 py-1 text-xs rounded-full border transition-all',
                  filter === cat
                    ? 'bg-primary/10 border-primary/40 text-primary'
                    : 'border-border text-muted-foreground hover:bg-accent'
                )}
              >
                {cat === 'all' ? t('deliverables.all') : cat}
              </button>
            ))}
          </div>
        </div>

        {/* File list */}
        {loading ? (
          <div className="flex items-center justify-center h-40 text-muted-foreground text-sm">
            <RefreshCw size={16} className="animate-spin mr-2" />
            {t('common.loading')}
          </div>
        ) : filtered.length === 0 ? (
          <div className="flex flex-col items-center justify-center h-48 text-muted-foreground">
            <FolderOpen size={40} className="mb-3 opacity-30" />
            <p className="text-sm font-medium">{t('deliverables.empty')}</p>
            <p className="text-xs mt-1">{t('deliverables.emptyHint')}</p>
          </div>
        ) : (
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
            {filtered.map((file) => (
              <div
                key={file.path}
                className="group border border-border rounded-xl p-4 bg-card hover:border-border/80 hover:shadow-sm transition-all"
              >
                <div className="flex items-start gap-3">
                  <div className="shrink-0 mt-0.5">{getFileIcon(file.ext)}</div>
                  <div className="flex-1 min-w-0">
                    <p className="text-sm font-medium truncate" title={file.name}>
                      {file.name}
                    </p>
                    <div className="flex items-center gap-2 mt-0.5">
                      <span className="text-[11px] text-muted-foreground">{formatSize(file.size)}</span>
                      {file.modified && (
                        <span className="text-[11px] text-muted-foreground">{formatTime(file.modified)}</span>
                      )}
                    </div>
                    <span className="inline-block mt-1 text-[10px] px-1.5 py-0.5 bg-muted rounded text-muted-foreground">
                      {getFileCategory(file.ext)}
                    </span>
                  </div>
                  <button
                    onClick={() => handleDownload(file)}
                    className="shrink-0 p-1.5 rounded-lg opacity-0 group-hover:opacity-100 hover:bg-accent text-muted-foreground hover:text-foreground transition-all"
                    title={t('common.download')}
                  >
                    <Download size={14} />
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
