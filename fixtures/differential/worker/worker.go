package worker

type Worker struct{}

func (w *Worker) Step() {}

func (w *Worker) Run() { w.Step() }
