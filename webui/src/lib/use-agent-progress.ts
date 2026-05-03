import { useEffect, useState } from 'react';
import { wsManager } from './ws';

export interface AgentProgressState {
  task_id: string;
  progress_type: string;
  tokens_added: number;
  tools_added: number;
  total_tokens: number;
  total_tools: number;
  timestamp: number;
}

/** 任务阶段进度（阶段描述 + 百分比） */
export interface AgentStageState {
  task_id: string;
  stage: string;
  percent: number;
  timestamp: number;
}

/**
 * Hook to subscribe to agent progress events for a specific task.
 * Returns the latest progress state for the given task_id.
 */
export function useAgentProgress(taskId: string | undefined): AgentProgressState | null {
  const [progress, setProgress] = useState<AgentProgressState | null>(null);

  useEffect(() => {
    if (!taskId) {
      setProgress(null);
      return;
    }

    const unsubscribe = wsManager.on('agent_progress', (event) => {
      if (event.task_id === taskId && event.progress_type === 'delta') {
        setProgress({
          task_id: event.task_id!,
          progress_type: event.progress_type!,
          tokens_added: event.tokens_added ?? 0,
          tools_added: event.tools_added ?? 0,
          total_tokens: event.total_tokens ?? 0,
          total_tools: event.total_tools ?? 0,
          timestamp: Date.now(),
        });
      }
    });

    return unsubscribe;
  }, [taskId]);

  return progress;
}

/**
 * Hook to subscribe to all agent progress events.
 * Returns an array of progress states for all active tasks.
 */
export function useAllAgentProgress(): AgentProgressState[] {
  const [progressMap, setProgressMap] = useState<Map<string, AgentProgressState>>(new Map());

  useEffect(() => {
    const unsubscribe = wsManager.on('agent_progress', (event) => {
      if (event.progress_type === 'delta' && event.task_id) {
        setProgressMap((prev) => {
          const next = new Map(prev);
          next.set(event.task_id!, {
            task_id: event.task_id!,
            progress_type: event.progress_type!,
            tokens_added: event.tokens_added ?? 0,
            tools_added: event.tools_added ?? 0,
            total_tokens: event.total_tokens ?? 0,
            total_tools: event.total_tools ?? 0,
            timestamp: Date.now(),
          });
          // Keep only last 10 active progress entries
          if (next.size > 10) {
            const oldest = [...next.entries()]
              .sort((a, b) => a[1].timestamp - b[1].timestamp)[0][0];
            next.delete(oldest);
          }
          return next;
        });
      }
    });

    return unsubscribe;
  }, []);

  return [...progressMap.values()];
}

/**
 * Hook to subscribe to agent stage progress events for a specific task.
 * Returns the latest stage state (stage description + percent).
 */
export function useAgentStage(taskId: string | undefined): AgentStageState | null {
  const [stage, setStage] = useState<AgentStageState | null>(null);

  useEffect(() => {
    if (!taskId) {
      setStage(null);
      return;
    }

    const unsubscribe = wsManager.on('agent_stage', (event) => {
      if (event.task_id === taskId) {
        setStage({
          task_id: event.task_id!,
          stage: event.stage ?? '',
          percent: event.percent ?? 0,
          timestamp: Date.now(),
        });
      }
    });

    return unsubscribe;
  }, [taskId]);

  return stage;
}

/**
 * Hook to subscribe to all agent stage events.
 * Returns a map of task_id -> AgentStageState for all active tasks.
 */
export function useAllAgentStages(): Map<string, AgentStageState> {
  const [stageMap, setStageMap] = useState<Map<string, AgentStageState>>(new Map());

  useEffect(() => {
    const unsubscribe = wsManager.on('agent_stage', (event) => {
      if (event.task_id) {
        setStageMap((prev) => {
          const next = new Map(prev);
          next.set(event.task_id!, {
            task_id: event.task_id!,
            stage: event.stage ?? '',
            percent: event.percent ?? 0,
            timestamp: Date.now(),
          });
          // Keep only last 10 active stage entries (same limit as useAllAgentProgress)
          if (next.size > 10) {
            const oldest = [...next.entries()]
              .sort((a, b) => a[1].timestamp - b[1].timestamp)[0][0];
            next.delete(oldest);
          }
          return next;
        });
      }
    });

    return unsubscribe;
  }, []);

  return stageMap;
}
